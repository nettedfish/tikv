#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rocksdb::DB;
use tempdir::TempDir;

use tikv::raftserver::Result;
use tikv::raftserver::store::*;
use super::util::*;
use tikv::proto::raft_cmdpb::*;
use tikv::proto::metapb;
use tikv::proto::raftpb::ConfChangeType;
use tikv::pd::Client;
use super::pd::PdClient;

// We simulate 3 or 5 nodes, each has a store.
// Sometimes, we use fixed id to test, which means the id
// isn't allocated by pd, and node id, store id are same.
// E,g, for node 1, the node id and store id are both 1.

pub trait Simulator {
    // Pass 0 to let pd allocate a node id if db is empty.
    // If node id > 0, the node must be created in db already,
    // and the node id must be the same as given argument.
    // Return the node id.
    fn run_node(&mut self, node_id: u64, engine: Arc<DB>) -> u64;
    fn stop_node(&mut self, node_id: u64);
    fn get_node_ids(&self) -> HashSet<u64>;
    fn call_command(&self,
                    request: RaftCommandRequest,
                    timeout: Duration)
                    -> Option<RaftCommandResponse>;
}

pub struct Cluster<T: Simulator> {
    pub id: u64,
    leaders: HashMap<u64, metapb::Peer>,
    paths: Vec<TempDir>,
    dbs: Vec<Arc<DB>>,

    // node id -> db engine.
    pub engines: HashMap<u64, Arc<DB>>,

    sim: T,
    pub pd_client: Arc<RwLock<PdClient>>,
}

impl<T: Simulator> Cluster<T> {
    // Create the default Store cluster.
    pub fn new(id: u64, count: usize, sim: T, pd_client: Arc<RwLock<PdClient>>) -> Cluster<T> {
        let mut c = Cluster {
            id: id,
            leaders: HashMap::new(),
            paths: vec![],
            dbs: vec![],
            engines: HashMap::new(),
            sim: sim,
            pd_client: pd_client,
        };

        c.create_engines(count);

        c
    }

    fn create_engines(&mut self, count: usize) {
        for _ in 0..count {
            self.paths.push(TempDir::new("test_cluster").unwrap());
        }

        for item in &self.paths {
            self.dbs.push(new_engine(item));
        }
    }

    pub fn start(&mut self) {
        for engine in &self.dbs {
            let node_id = self.sim.run_node(0, engine.clone());
            self.engines.insert(node_id, engine.clone());
        }
    }

    pub fn run_node(&mut self, node_id: u64) {
        let engine = self.engines.get(&node_id).unwrap();
        self.sim.run_node(node_id, engine.clone());
    }

    pub fn stop_node(&mut self, node_id: u64) {
        self.sim.stop_node(node_id);
    }

    pub fn get_engine(&self, node_id: u64) -> Arc<DB> {
        self.engines.get(&node_id).unwrap().clone()
    }

    pub fn call_command(&self,
                        request: RaftCommandRequest,
                        timeout: Duration)
                        -> Option<RaftCommandResponse> {
        self.sim.call_command(request, timeout)
    }

    pub fn call_command_on_leader(&mut self,
                                  region_id: u64,
                                  mut request: RaftCommandRequest,
                                  timeout: Duration)
                                  -> Option<RaftCommandResponse> {
        request.mut_header().set_peer(self.leader_of_region(region_id).clone().unwrap());
        self.call_command(request, timeout)
    }

    pub fn leader_of_region(&mut self, region_id: u64) -> Option<metapb::Peer> {
        if let Some(l) = self.leaders.get(&region_id) {
            return Some(l.clone());
        }
        let mut leader = None;
        let mut retry_cnt = 100;

        let stores = self.pd_client.read().unwrap().get_stores(self.id).unwrap();
        let node_ids: HashSet<u64> = self.sim.get_node_ids();
        while leader.is_none() && retry_cnt > 0 {
            for store in &stores {
                // For some tests, we stop the node but pd still has this information,
                // and we must skip this.
                if !node_ids.contains(&store.get_node_id()) {
                    continue;
                }

                // To get region leader, we don't care real peer id, so use 0 instead.
                let peer = new_peer(store.get_node_id(), store.get_store_id(), 0);
                let find_leader = new_status_request(region_id, &peer, new_region_leader_cmd());
                let resp = self.call_command(find_leader, Duration::from_secs(3)).unwrap();
                let region_leader = resp.get_status_response().get_region_leader();
                if region_leader.has_leader() {
                    leader = Some(region_leader.get_leader().clone());
                    break;
                }
            }
            sleep_ms(10);
            retry_cnt -= 1;
        }

        if let Some(l) = leader {
            self.leaders.insert(region_id, l);
        }

        self.leaders.get(&region_id).cloned()
    }

    // Multiple nodes with fixed node id, like node 1, 2, .. 5,
    // First region 1 is in all stores with peer 1, 2, .. 5.
    // Peer 1 is in node 1, store 1, etc.
    pub fn bootstrap_region(&mut self) -> Result<()> {
        for (id, engine) in self.dbs.iter().enumerate() {
            let id = id as u64 + 1;
            self.engines.insert(id, engine.clone());
        }

        let mut region = metapb::Region::new();
        region.set_region_id(1);
        region.set_start_key(keys::MIN_KEY.to_vec());
        region.set_end_key(keys::MAX_KEY.to_vec());

        for (&id, engine) in &self.engines {
            let peer = new_peer(id, id, id);
            region.mut_peers().push(peer.clone());
            bootstrap_store(engine.clone(), self.id, id, id).unwrap();
        }

        for engine in self.engines.values() {
            try!(write_region(&engine, &region));
        }

        self.bootstrap_cluster(region);

        Ok(())
    }

    // Multiple nodes with fixed node id, like node 1, 2, .. 5.
    // First region 1 is only in node 1, store 1 with peer 1.
    pub fn bootstrap_conf_change(&mut self) {
        for (id, engine) in self.dbs.iter().enumerate() {
            let id = id as u64 + 1;
            self.engines.insert(id, engine.clone());
        }

        for (&id, engine) in &self.engines {
            bootstrap_store(engine.clone(), self.id, id, id).unwrap();
        }

        let node_id = 1;
        let region = bootstrap_region(self.engines.get(&node_id).unwrap().clone(), 1, 1, 1, 1)
                         .unwrap();
        self.bootstrap_cluster(region);
    }

    // This is only for fixed id test.
    fn bootstrap_cluster(&mut self, region: metapb::Region) {
        self.pd_client
            .write()
            .unwrap()
            .bootstrap_cluster(self.id,
                               new_node(1, "".to_owned()),
                               vec![new_store(1, 1)],
                               region)
            .unwrap();

        for &id in self.engines.keys() {
            self.pd_client.write().unwrap().put_node(self.id, new_node(id, "".to_owned())).unwrap();
            self.pd_client.write().unwrap().put_store(self.id, new_store(id, id)).unwrap();
        }
    }

    pub fn reset_leader_of_region(&mut self, region_id: u64) {
        self.leaders.remove(&region_id);
    }

    pub fn check_quorum<F: FnMut(&&Arc<DB>) -> bool>(&self, condition: F) -> bool {
        if self.engines.is_empty() {
            return true;
        }
        self.engines.values().filter(condition).count() > self.engines.len() / 2
    }

    pub fn shutdown(&mut self) {
        let keys: HashSet<u64> = self.sim.get_node_ids();
        for id in keys {
            self.stop_node(id);
        }
        self.leaders.clear();
    }

    // If the resp is "not leader error", get the real leader.
    // Sometimes, we may still can't get leader even in "not leader error",
    // returns a INVALID_PEER for this.
    pub fn refresh_leader_if_needed(&mut self, resp: &RaftCommandResponse, region_id: u64) -> bool {
        if !is_error_response(resp) {
            return false;
        }

        let err = resp.get_header().get_error().get_detail();
        if !err.has_not_leader() {
            return false;
        }

        let err = err.get_not_leader();
        if !err.has_leader() {
            return false;
        }
        self.leaders.insert(region_id, err.get_leader().clone());
        true
    }

    pub fn request(&mut self,
                   region_id: u64,
                   request: RaftCommandRequest,
                   timeout: Duration)
                   -> RaftCommandResponse {
        loop {
            let resp = self.call_command_on_leader(region_id, request.clone(), timeout).unwrap();
            if !resp.get_header().has_error() || !self.refresh_leader_if_needed(&resp, region_id) {
                return resp;
            }
            error!("refreshed leader of region {}", region_id);
        }
    }

    pub fn get_region(&self, key: &[u8]) -> metapb::Region {
        self.pd_client
            .read()
            .unwrap()
            .get_region(self.id, key)
            .unwrap()
    }

    pub fn get_region_id(&self, key: &[u8]) -> u64 {
        self.get_region(key).get_region_id()
    }

    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let region_id = self.get_region_id(key);
        let get = new_request(region_id, vec![new_get_cmd(&keys::data_key(key))]);
        let mut resp = self.request(region_id, get, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CommandType::Get);
        let mut get = resp.mut_responses()[0].take_get();
        if get.has_value() {
            Some(get.take_value())
        } else {
            None
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        let region_id = self.get_region_id(key);
        let put = new_request(region_id, vec![new_put_cmd(&keys::data_key(key), value)]);
        let resp = self.request(region_id, put, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CommandType::Put);
    }

    pub fn seek(&mut self, key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let region_id = self.get_region_id(key);
        let seek = new_request(region_id, vec![new_seek_cmd(&keys::data_key(key))]);
        let resp = self.request(region_id, seek, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        let resp = &resp.get_responses()[0];
        assert_eq!(resp.get_cmd_type(), CommandType::Seek);
        if !resp.has_seek() {
            None
        } else {
            Some((resp.get_seek().get_key().to_vec(),
                  resp.get_seek().get_value().to_vec()))
        }
    }

    pub fn delete(&mut self, key: &[u8]) {
        let region_id = self.get_region_id(key);
        let delete = new_request(region_id, vec![new_delete_cmd(&keys::data_key(key))]);
        let resp = self.request(region_id, delete, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CommandType::Delete);
    }

    pub fn change_peer(&mut self,
                       region_id: u64,
                       change_type: ConfChangeType,
                       peer: metapb::Peer) {
        let change_peer = new_admin_request(region_id, new_change_peer_cmd(change_type, peer));
        let resp = self.call_command_on_leader(region_id, change_peer, Duration::from_secs(3))
                       .unwrap();
        assert_eq!(resp.get_admin_response().get_cmd_type(),
                   AdminCommandType::ChangePeer);

        let region = resp.get_admin_response().get_change_peer().get_region();
        self.pd_client.write().unwrap().update_region(self.id, region.clone()).unwrap();
    }
}

impl<T: Simulator> Drop for Cluster<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}