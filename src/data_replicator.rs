use crdt;
use packet;
use result::Result;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use streamer;

pub struct DataReplicator {
    pub thread_hdls: Vec<JoinHandle<()>>,
}

impl DataReplicator {
    pub fn new(
        crdt: Arc<RwLock<crdt::Crdt>>,
        window: Arc<RwLock<Vec<Option<packet::SharedBlob>>>>,
        gossip_listen_socket: UdpSocket,
        gossip_send_socket: UdpSocket,
        exit: Arc<AtomicBool>,
    ) -> Result<DataReplicator> {
        let blob_recycler = packet::BlobRecycler::default();
        let (request_sender, request_receiver) = channel();
        trace!(
            "DataReplicator: id: {:?}, listening on: {:?}",
            &crdt.read().unwrap().me[..4],
            gossip_listen_socket.local_addr().unwrap()
        );
        let t_receiver = streamer::blob_receiver(
            exit.clone(),
            blob_recycler.clone(),
            gossip_listen_socket,
            request_sender,
        )?;
        let (response_sender, response_receiver) = channel();
        let t_responder = streamer::responder(
            gossip_send_socket,
            exit.clone(),
            blob_recycler.clone(),
            response_receiver,
        );
        let t_listen = crdt::Crdt::listen(
            crdt.clone(),
            window,
            blob_recycler.clone(),
            request_receiver,
            response_sender.clone(),
            exit.clone(),
        );
        let t_gossip = crdt::Crdt::gossip(crdt.clone(), blob_recycler, response_sender, exit);
        let thread_hdls = vec![t_receiver, t_responder, t_listen, t_gossip];
        Ok(DataReplicator { thread_hdls })
    }
}

#[cfg(test)]
mod tests {
    use crdt::{Crdt, TestNode};
    use data_replicator::DataReplicator;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, RwLock};

    #[test]
    // test that stage will exit when flag is set
    fn test_exit() {
        let exit = Arc::new(AtomicBool::new(false));
        let tn = TestNode::new();
        let crdt = Crdt::new(tn.data.clone());
        let c = Arc::new(RwLock::new(crdt));
        let w = Arc::new(RwLock::new(vec![]));
        let d = DataReplicator::new(
            c.clone(),
            w,
            tn.sockets.gossip,
            tn.sockets.gossip_send,
            exit.clone(),
        ).unwrap();
        exit.store(true, Ordering::Relaxed);
        for t in d.thread_hdls {
            t.join().expect("thread join");
        }
    }
}
