use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::{Duration, SystemTime},
};

use blockchain::{Block, BlockIterator, Blockchain};
use client::{ClientInfo, ClientWriter};
pub use error::{Error, Result};
use indexmap::{IndexMap, IndexSet};
use k256::{
    elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
    SecretKey,
};
use message::Message;
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{broadcast, mpsc, Mutex},
    time,
};

mod blockchain;
mod client;
mod error;
mod message;

#[macro_export]
macro_rules! ex {
    ($e: expr, $name: tt) => {
        $e.map_err(|e| Error::$name(line!(), module_path!(), e))?
    };
}

pub type PubKeyBytes = [u8; 33];
pub type HashBytes = [u8; 32];
pub type SignBytes = [u8; 64];

#[derive(Clone)]
pub struct Config {
    pub addr: String,
    pub folder: PathBuf,
    pub keep_alive: Duration,
    pub data_gather_time: Duration,
    pub thin: bool,
    pub relationship_time: Duration,
    pub relationship_count: u32,
}

type Clients = HashMap<PubKeyBytes, Arc<ClientInfo>>;

pub struct Peer {
    me: Weak<Peer>,
    pub cfg: Config,
    prikey: SecretKey,
    pubkey: PubKeyBytes,
    pubhex: String,
    to_accept: mpsc::Sender<SocketAddr>,
    last_block_tx: broadcast::Sender<Block>,
    to_shutdown: Arc<AtomicBool>,
    clients: Mutex<Clients>,
    known: Mutex<IndexMap<PubKeyBytes, SystemTime>>,
    blockchain: Mutex<Blockchain>,
}

impl Peer {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        let prikey = k256::SecretKey::random(&mut OsRng);
        let mut pubkey: PubKeyBytes = [0u8; 33];
        pubkey.copy_from_slice(prikey.public_key().to_encoded_point(true).as_bytes());
        let to_shutdown = Arc::new(AtomicBool::new(false));

        let blockchain = ex!(Blockchain::new(&cfg.folder), source);

        let pubhex: String = hex::encode(&pubkey);

        tracing::info!(
            "root block {}",
            blockchain
                .root
                .as_ref()
                .map(|r| hex::encode(r))
                .unwrap_or(String::new())
        );
        tracing::info!(
            "last block {} {}",
            blockchain.count,
            blockchain
                .last
                .as_ref()
                .map(|r| hex::encode(r))
                .unwrap_or(String::new())
        );
        let (last_block_tx, _) = broadcast::channel(10);
        let (to_accept_tx, to_accept_rx) = mpsc::channel(10);

        let peer = Arc::new_cyclic(|me| Self {
            // let peer = Arc::new(Self {
            me: me.clone(),
            prikey,
            pubkey,
            pubhex,
            cfg,
            to_accept: to_accept_tx,
            last_block_tx,
            to_shutdown,
            clients: Mutex::new(HashMap::new()),
            known: Mutex::new(IndexMap::new()),
            blockchain: Mutex::new(blockchain),
        });

        let p0 = peer.clone();
        tokio::spawn(async move {
            if let Err(e) = p0.listen(to_accept_rx).await {
                tracing::error!("{} Loop: {e}", p0.pubhex);
            }
        });

        if !peer.cfg.thin {
            let p1 = peer.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(p1.cfg.keep_alive);
                let msg = Message::keepalive(&p1.pubkey, &p1.prikey);

                while !p1.to_shutdown.load(Ordering::SeqCst) {
                    interval.tick().await;

                    if let Err(e) = p1.broadcast(msg.clone()).await {
                        tracing::error!("{} {}", p1.pubhex, e);
                    }

                    p1.known.lock().await.retain(|_k, v| {
                        if let Ok(elapsed) = v.elapsed() {
                            elapsed < p1.cfg.keep_alive
                        } else {
                            false
                        }
                    });
                }
            });

            let p2 = peer.clone();
            tokio::spawn(async move {
                if let Err(e) = p2.establish_relationship().await {
                    tracing::error!("{} {}", p2.pubhex, e);
                }
            });
        }

        Ok(peer)
    }

    async fn establish_relationship(&self) -> Result<()> {
        let mut interval = time::interval(self.cfg.relationship_time);
        while !self.to_shutdown.load(Ordering::SeqCst) {
            interval.tick().await;
            {
                let current = self.clients.lock().await.len();

                if current < self.cfg.relationship_count as _ {
                    let keys: Vec<PubKeyBytes> = self.clients.lock().await.keys().cloned().collect();
                    ex!(
                        self.broadcast(Message::RequestNeighbours {
                            count: self.cfg.relationship_count,
                            exclude: keys,
                        })
                        .await,
                        source
                    );
                }
            };
        }

        Ok(())
    }

    async fn broadcast_except(&self, msg: Message, except: &Arc<ClientInfo>) -> Result<()> {
        let except = except.pubkey;
        let cls = self.clients.lock().await;
        for to in cls.values() {
            if to.pubkey != except {
                ex!(to.write(&msg).await, source);
            }
        }

        Ok(())
    }

    async fn broadcast(&self, msg: Message) -> Result<()> {
        let cls = self.clients.lock().await;
        for to in cls.values() {
            ex!(to.write(&msg).await, source);
        }

        Ok(())
    }

    async fn listen(&self, mut to_accept: mpsc::Receiver<SocketAddr>) -> Result<()> {
        tracing::info!("{} start", self.pubhex);

        let listener = ex!(TcpListener::bind(self.cfg.addr.clone()).await, io);

        while !self.to_shutdown.load(Ordering::SeqCst) {
            select! {
                addr = to_accept.recv() => {
                    if let Some(addr) = addr {
                        let sck = ex!(TcpStream::connect(addr).await, io);
                        ex!(self.accept(addr, sck).await, source);
                    }
                }
                res = listener.accept() => {
                    match res {
                        Ok((sck, addr)) => {
                            ex!(self.accept(addr, sck).await, source);
                        }
                        Err(e) => {
                            tracing::error!("{} {}", self.pubhex, e);
                        }
                    }
                }
            }
        }

        tracing::info!("{} shutdown", self.pubhex);

        Ok(())
    }

    async fn accept(&self, addr: SocketAddr, sck: TcpStream) -> Result<()> {
        tracing::info!("{} accept {}", self.pubhex, addr);

        let (mut reader, writer) = sck.into_split();
        let mut clw = ClientWriter {
            shared: None,
            sck: writer,
            tx_nonce: 0,
        };

        let greeting = {
            let blkch = self.blockchain.lock().await;
            Message::Greeting {
                pubkey: self.pubkey.clone(),
                listen: self.cfg.addr.parse().unwrap(),
                root: blkch.root.clone(),
                last: blkch.last.clone(),
                count: blkch.count,
                thin: self.cfg.thin,
            }
        };

        ex!(clw.write(&greeting).await, source);

        let (nonce, greeting) = ex!(ClientWriter::read(&mut reader, &None).await, source);

        if let Message::Greeting {
            pubkey,
            listen,
            root,
            last: _,
            count,
            thin,
        } = greeting
        {
            tracing::info!(
                "{} greeting {} {} {}",
                self.pubhex,
                hex::encode(&pubkey),
                root.as_ref().map(|r| hex::encode(r)).unwrap_or("".into()),
                count
            );
            let shared = clw.shared_secret(&pubkey, &self.prikey);
            let cl = ClientInfo {
                addr,
                listen,
                pubkey,
                writer: Mutex::new(clw),
            };

            if !thin {
                self.known.lock().await.insert(pubkey, SystemTime::now());
            }

            let mut blkch = self.blockchain.lock().await;

            if !self.cfg.thin && !thin && root.is_none() && blkch.root.is_none() {
                if self.pubkey > pubkey {
                    tracing::info!("{} create genesis block", self.pubhex);
                    blkch.cache.insert(b"[genesis]".to_vec());
                    let blk = ex!(self.create_next_block(&mut *blkch).await, source);
                    ex!(cl.write(&Message::ShareBlock { block: blk }).await, source);
                }
            } else if blkch.root.is_none() {
                ex!(cl.write(&Message::RequestBlocks { start: None }).await, source);
            } else if count > blkch.count {
                ex!(
                    cl.write(&Message::RequestBlocks {
                        start: blkch.last.clone()
                    })
                    .await,
                    source
                );
            }

            let pubkey = cl.pubkey;
            let pubhex = self.pubhex.clone();
            let cl = Arc::new(cl);

            let cl0 = cl.clone();
            self.clients.lock().await.insert(pubkey, cl0);

            let peer = self.me.upgrade().unwrap();

            tokio::spawn(async move {
                let mut rx_nonce = nonce;
                loop {
                    let msg = ClientWriter::read(&mut reader, &shared).await;
                    match msg {
                        Ok((nonce, msg)) => {
                            if nonce > rx_nonce {
                                rx_nonce = nonce;
                                if let Err(e) = peer.on_message(msg, cl.clone()).await {
                                    tracing::error!("{} {}", pubhex, e);
                                }
                            } else {
                                tracing::warn!("{} nonce to low, omit message", pubhex);
                            }
                        }
                        Err(e) => {
                            tracing::error!("{} {}", pubhex, e);
                            break;
                        }
                    }
                }

                peer.clients.lock().await.remove(&pubkey);
            });
        } else {
            return Err(Error::protocol(
                line!(),
                module_path!(),
                "first message was not greeting",
            ));
        }

        Ok(())
    }

    async fn create_next_block(&self, blkch: &mut Blockchain) -> Result<Block> {
        tracing::info!("{} create next block", self.pubhex);

        let next_author = {
            let k = self.known.lock().await;
            k.get_index(rand::random::<usize>() % k.len())
                .map(|(k, _v)| k.clone())
                .unwrap()
        };
        let blk = ex!(
            blkch.create_block(next_author, self.pubkey.clone(), &self.prikey),
            source
        );
        ex!(blkch.add_block(blk.clone()), source);

        Ok(blk)
    }

    async fn on_message(&self, msg: Message, cl: Arc<ClientInfo>) -> Result<()> {
        match msg {
            Message::Greeting { .. } => {
                tracing::error!("{} we should never get a second greeting", self.pubhex);
            }
            m @ Message::KeepAlive { pubkey, .. } => {
                if pubkey == self.pubkey {
                    return Ok(());
                }

                if ex!(m.verify(), source) {
                    let previous = self.known.lock().await.insert(pubkey.clone(), SystemTime::now());

                    if let Some(old) = previous {
                        // tracing::debug!("{} keep alive {}", self.pubhex, hex::encode(&pubkey));
                        let elapsed = old.elapsed().unwrap_or(self.cfg.keep_alive);
                        let delta = if elapsed >= self.cfg.keep_alive {
                            0
                        } else {
                            (self.cfg.keep_alive - elapsed).as_millis()
                        };

                        if delta < 50 {
                            ex!(self.broadcast_except(m, &cl).await, source);
                        }
                    } else if previous.is_none() {
                        ex!(self.broadcast_except(m, &cl).await, source);
                    }
                }
            }
            Message::ShareData { data } => {
                tracing::info!("{} got data", self.pubhex);

                let unknown = self.blockchain.lock().await.cache.insert(data.clone());
                if unknown {
                    let msg = Message::ShareData { data };
                    ex!(self.broadcast_except(msg, &cl).await, source);
                }
            }
            Message::RequestBlocks { start } => {
                tracing::info!(
                    "{} request for blocks {}",
                    self.pubhex,
                    start.map(|n| hex::encode(n)).unwrap_or(String::new())
                );
                let blk_it = self.blockchain.lock().await.get_blocks(start);
                for block in blk_it {
                    let block = ex!(block, source);
                    ex!(cl.write(&Message::RequestedBlock { block }).await, source);
                }
            }
            Message::RequestedBlock { block } => {
                tracing::info!("{} got block {}", self.pubhex, hex::encode(&block.hash));

                ex!(self.blockchain.lock().await.add_block(block.clone()), source);
                if self.last_block_tx.receiver_count() > 0 {
                    ex!(self.last_block_tx.send(block), sync);
                }
            }
            Message::ShareBlock { block } => {
                tracing::info!("{} share block {}", self.pubhex, hex::encode(&block.hash));
                let last = self.blockchain.lock().await.last;
                if last.map(|n| n == block.hash).unwrap_or(false) {
                    // we get the same block again, just ignore it
                    return Ok(());
                }

                if self.pubkey == block.next_choice {
                    tracing::info!("{} me is next xD", self.pubhex);

                    let peer = self.me.upgrade().unwrap();
                    tokio::spawn(async move {
                        let mut interval = time::interval(peer.cfg.data_gather_time);
                        let res: Result<()> = async {
                            loop {
                                interval.tick().await;

                                let mut blkch = peer.blockchain.lock().await;
                                if blkch.cache.len() > 0 {
                                    let block = ex!(peer.create_next_block(&mut *blkch).await, source);
                                    ex!(
                                        peer.broadcast(Message::ShareBlock { block: block.clone() }).await,
                                        source
                                    );
                                    if peer.last_block_tx.receiver_count() > 0 {
                                        ex!(peer.last_block_tx.send(block), sync);
                                    }
                                    break;
                                }
                            }

                            Ok(())
                        }
                        .await;

                        if let Err(e) = res {
                            tracing::error!("{} {}", peer.pubhex, e);
                        }
                    });
                }
                ex!(self.blockchain.lock().await.add_block(block.clone()), source);

                ex!(
                    self.broadcast_except(Message::ShareBlock { block: block.clone() }, &cl)
                        .await,
                    source
                );
                if self.last_block_tx.receiver_count() > 0 {
                    ex!(self.last_block_tx.send(block), sync);
                }
            }
            Message::RequestNeighbours { count, exclude } => {
                let cls = self.clients.lock().await;
                let mut exclude: HashSet<PubKeyBytes> = exclude.into_iter().collect();
                let mut to_share = Vec::new();

                exclude.insert(cl.pubkey);

                // let to_exclude: Vec<String> = exclude.iter().map(|x| hex::encode(x)).collect();
                // let to_exclude = to_exclude.join("\n");
                // tracing::info!(
                //     "{} request for neighbours\nfrom: {}\n{}",
                //     self.pubhex,
                //     hex::encode(cl.pubkey),
                //     to_exclude
                // );

                for (k, cl) in cls.iter() {
                    if !exclude.contains(k) && to_share.len() < count as _ {
                        let listen = cl.listen;
                        to_share.push((k.clone(), listen));
                    }
                }

                if to_share.len() > 0 {
                    ex!(
                        cl.write(&Message::IntroduceNeighbours { neighbours: to_share }).await,
                        source
                    );
                }
            }
            Message::IntroduceNeighbours { neighbours } => {
                // let to_connect: Vec<String> = neighbours.iter().map(|x| hex::encode(x.0)).collect();
                // let to_connect = to_connect.join("\n");
                // tracing::info!(
                //     "{} introduce new neighbours {}\n{}",
                //     self.pubhex,
                //     neighbours.len(),
                //     to_connect
                // );
                let cnt = self.clients.lock().await.len();
                if cnt < self.cfg.relationship_count as _ {
                    let to_add = self.cfg.relationship_count as usize - cnt;
                    for (_k, n) in neighbours.into_iter().take(to_add) {
                        ex!(self.to_accept.send(n).await, sync);
                    }
                }
            }
        }

        Ok(())
    }

    pub fn pubhex(&self) -> String {
        self.pubhex.clone()
    }

    pub async fn client_pubkeys(&self) -> HashSet<PubKeyBytes> {
        let cl = self.clients.lock().await;
        cl.keys().cloned().collect()
    }

    pub async fn known_pubkeys(&self) -> IndexSet<PubKeyBytes> {
        self.known.lock().await.keys().cloned().collect()
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<()> {
        tracing::info!("{} connect to {}", self.pubhex, addr);
        ex!(self.to_accept.send(addr).await, sync);
        Ok(())
    }

    pub async fn share(&self, data: Vec<u8>) -> Result<()> {
        let unknown = self.blockchain.lock().await.cache.insert(data.clone());
        if unknown {
            let msg = Message::ShareData { data };
            ex!(self.broadcast(msg).await, source);
        }

        Ok(())
    }

    pub fn last_block_receiver(&self) -> broadcast::Receiver<Block> {
        self.last_block_tx.subscribe()
    }

    pub async fn block_iter(&self) -> BlockIterator {
        self.blockchain.lock().await.get_blocks(None)
    }

    pub fn shutdown(&self) {
        self.to_shutdown.store(true, Ordering::SeqCst);
    }
}
