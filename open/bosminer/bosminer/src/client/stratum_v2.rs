// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use ii_logging::macros::*;

use crate::job;
use crate::node;
use crate::stats;
use crate::sync;
use crate::work;

use ii_bitcoin::HashTrait;

use bosminer_config::client;
use bosminer_macros::ClientNode;

use async_trait::async_trait;
use futures::lock::Mutex;
use ii_async_compat::join;
use ii_async_compat::prelude::*;

use std::collections::VecDeque;
use std::fmt;
use std::net::ToSocketAddrs;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use ii_stratum::v2::framing::Framing;
use ii_stratum::v2::messages::{
    NewMiningJob, OpenStandardMiningChannel, OpenStandardMiningChannelError,
    OpenStandardMiningChannelSuccess, SetNewPrevHash, SetTarget, SetupConnection,
    SetupConnectionError, SetupConnectionSuccess, SubmitSharesError, SubmitSharesStandard,
    SubmitSharesSuccess,
};
use ii_stratum::v2::types::DeviceInfo;
use ii_stratum::v2::types::*;
use ii_stratum::v2::{build_message_from_frame, Handler, Protocol};
use ii_wire::{Connection, ConnectionRx, ConnectionTx, Message};

use std::collections::HashMap;

// TODO: move it to the stratum crate
const VERSION_MASK: u32 = 0x1fffe000;

#[derive(Debug)]
pub struct ConnectionDetails {
    pub user: String,
    pub host: String,
    pub port: u16,
}

impl ConnectionDetails {
    fn get_host_and_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl From<client::Descriptor> for ConnectionDetails {
    fn from(descriptor: client::Descriptor) -> Self {
        Self {
            user: descriptor.user,
            host: descriptor.host,
            port: descriptor.port,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StratumJob {
    client: Arc<StratumClient>,
    id: u32,
    channel_id: u32,
    version: u32,
    prev_hash: ii_bitcoin::DHash,
    merkle_root: ii_bitcoin::DHash,
    time: u32,
    bits: u32,
    target: ii_bitcoin::Target,
}

impl StratumJob {
    pub fn new(
        client: Arc<StratumClient>,
        job_msg: &NewMiningJob,
        prevhash_msg: &SetNewPrevHash,
        target: ii_bitcoin::Target,
    ) -> Self {
        Self {
            client,
            id: job_msg.job_id,
            channel_id: job_msg.channel_id,
            version: job_msg.version,
            prev_hash: ii_bitcoin::DHash::from_slice(prevhash_msg.prev_hash.as_ref())
                .expect("BUG: Stratum: incorrect size of prev hash"),
            merkle_root: ii_bitcoin::DHash::from_slice(job_msg.merkle_root.as_ref())
                .expect("BUG: Stratum: incorrect size of merkle root"),
            time: prevhash_msg.min_ntime,
            bits: prevhash_msg.nbits,
            target,
        }
    }
}

impl job::Bitcoin for StratumJob {
    fn origin(&self) -> Arc<dyn node::Client> {
        self.client.clone()
    }

    fn version(&self) -> u32 {
        self.version
    }

    fn version_mask(&self) -> u32 {
        VERSION_MASK
    }

    fn previous_hash(&self) -> &ii_bitcoin::DHash {
        &self.prev_hash
    }

    fn merkle_root(&self) -> &ii_bitcoin::DHash {
        &self.merkle_root
    }

    fn time(&self) -> u32 {
        self.time
    }

    fn bits(&self) -> u32 {
        self.bits
    }

    fn target(&self) -> ii_bitcoin::Target {
        self.target
    }

    fn is_valid(&self) -> bool {
        // TODO: currently there is no easy way to detect the job is valid -> we have to check
        //  its presence in the registry. The inequality below was possible in the previous
        //  iteration of the protocol
        // self.block_height >= self.current_block_height.load(Ordering::Relaxed)
        true
    }
}

/// Queue that contains pairs of solution and its assigned sequence number. It is our responsibility
/// to keep the sequence number monotonic so that we as a stratum V2 client can easily process bulk
/// acknowledgements. The sequence number type has been selected as u32 to match
/// up with the protocol.
type SolutionQueue = Mutex<VecDeque<(work::Solution, u32)>>;

/// Helper task for `StratumClient` that implements Stratum V2 visitor which processes incoming
/// messages from remote server.
struct StratumEventHandler {
    client: Arc<StratumClient>,
    connection_rx: ConnectionRx<Framing>,
    job_sender: job::Sender,
    all_jobs: HashMap<u32, NewMiningJob>,
    current_prevhash_msg: Option<SetNewPrevHash>,
    /// Mining target for the next job that is to be solved
    current_target: ii_bitcoin::Target,
}

impl StratumEventHandler {
    pub fn new(
        client: Arc<StratumClient>,
        connection_rx: ConnectionRx<Framing>,
        job_sender: job::Sender,
        current_target: ii_bitcoin::Target,
    ) -> Self {
        Self {
            client,
            connection_rx,
            job_sender,
            all_jobs: Default::default(),
            current_prevhash_msg: None,
            current_target,
        }
    }

    /// Convert new mining job message into StratumJob and send it down the line for solving.
    ///
    /// * `job_msg` - job message used as a base for the StratumJob
    async fn update_job(&mut self, job_msg: &NewMiningJob) {
        let job = Arc::new(StratumJob::new(
            self.client.clone(),
            job_msg,
            self.current_prevhash_msg.as_ref().expect("no prevhash"),
            self.current_target,
        ));
        self.client.update_last_job(job.clone()).await;
        self.job_sender.send(job);
    }

    fn update_target(&mut self, value: Uint256Bytes) {
        let new_target: ii_bitcoin::Target = value.into();
        info!(
            "Stratum: changing target to {} diff={}",
            new_target,
            new_target.get_difficulty()
        );
        self.current_target = new_target;
    }

    async fn process_accepted_shares(&self, success_msg: &SubmitSharesSuccess) {
        let now = std::time::Instant::now();
        while let Some((solution, seq_num)) = self.client.solutions.lock().await.pop_front() {
            info!(
                "Stratum: accepted solution #{} with nonce={:08x}",
                seq_num,
                solution.nonce()
            );
            self.client
                .client_stats
                .accepted
                .account_solution(&solution.job_target(), now)
                .await;
            if success_msg.last_seq_num == seq_num {
                // all accepted solutions have been found
                return;
            }
        }
        warn!(
            "Stratum: last accepted solution #{} hasn't been found!",
            success_msg.last_seq_num
        );
    }

    async fn process_rejected_shares(&self, error_msg: &SubmitSharesError) {
        let now = std::time::Instant::now();
        while let Some((solution, seq_num)) = self.client.solutions.lock().await.pop_front() {
            if error_msg.seq_num == seq_num {
                info!(
                    "Stratum: rejected solution #{} with nonce={:08x}!",
                    seq_num,
                    solution.nonce()
                );
                self.client
                    .client_stats
                    .rejected
                    .account_solution(&solution.job_target(), now)
                    .await;
                // the rejected solution has been found
                return;
            } else {
                // TODO: this is currently not according to stratum V2 specification
                // preceding solutions are treated as accepted
                info!(
                    "Stratum: accepted solution #{} with nonce={}",
                    seq_num,
                    solution.nonce()
                );
                self.client
                    .client_stats
                    .accepted
                    .account_solution(&solution.job_target(), now)
                    .await;
                warn!(
                    "Stratum: the solution #{} precedes rejected solution #{}!",
                    seq_num, error_msg.seq_num
                );
                warn!(
                    "Stratum: the solution #{} is treated as an accepted one",
                    seq_num
                );
            }
        }
        warn!(
            "Stratum: rejected solution #{} hasn't been found!",
            error_msg.seq_num
        );
    }

    async fn run(mut self) -> job::Sender {
        while let Some(frame) = self.connection_rx.next().await {
            let msg = build_message_from_frame(frame)
                .expect("BUG: handle building V2 message from frame failed");
            msg.accept(&mut self).await;
        }
        // Return back job sender after terminating
        self.job_sender
    }
}

#[async_trait]
impl Handler for StratumEventHandler {
    // The rules for prevhash/mining job pairing are (currently) as follows:
    //  - when mining job comes
    //      - store it (by id)
    //      - start mining it if it doesn't have the future_job flag set
    //  - when prevhash message comes
    //      - replace it
    //      - start mining the job it references (by job id)
    //      - flush all other jobs

    async fn visit_new_mining_job(&mut self, _msg: &Message<Protocol>, job_msg: &NewMiningJob) {
        // all jobs since last `prevmsg` have to be stored in job table
        self.all_jobs.insert(job_msg.job_id, job_msg.clone());
        // TODO: close connection when maximal capacity of `all_jobs` has been reached

        // When not marked as future job, we can start mining on it right away
        if !job_msg.future_job {
            self.update_job(job_msg).await;
        }
    }

    async fn visit_set_new_prev_hash(
        &mut self,
        _msg: &Message<Protocol>,
        prevhash_msg: &SetNewPrevHash,
    ) {
        self.current_prevhash_msg.replace(prevhash_msg.clone());

        // find the future job with ID referenced in prevhash_msg
        let (_, mut future_job_msg) = self
            .all_jobs
            .remove_entry(&prevhash_msg.job_id)
            .expect("requested job ID not found");

        // remove all other jobs (they are now invalid)
        self.all_jobs.retain(|_, _| true);
        // turn the job into an immediate job
        future_job_msg.future_job = false;
        // reinsert the job
        self.all_jobs
            .insert(future_job_msg.job_id, future_job_msg.clone());

        // and start immediately solving it
        self.update_job(&future_job_msg).await;
    }

    async fn visit_set_target(&mut self, _msg: &Message<Protocol>, target_msg: &SetTarget) {
        self.update_target(target_msg.max_target);
    }

    async fn visit_submit_shares_success(
        &mut self,
        _msg: &Message<Protocol>,
        success_msg: &SubmitSharesSuccess,
    ) {
        self.process_accepted_shares(success_msg).await;
    }

    async fn visit_submit_shares_error(
        &mut self,
        _msg: &Message<Protocol>,
        error_msg: &SubmitSharesError,
    ) {
        self.process_rejected_shares(error_msg).await;
    }
}

struct StratumSolutionHandler {
    client: Arc<StratumClient>,
    connection_tx: ConnectionTx<Framing>,
    solution_receiver: job::SolutionReceiver,
    seq_num: u32,
}

impl StratumSolutionHandler {
    fn new(
        client: Arc<StratumClient>,
        connection_tx: ConnectionTx<Framing>,
        solution_receiver: job::SolutionReceiver,
    ) -> Self {
        Self {
            client,
            connection_tx,
            solution_receiver,
            seq_num: 0,
        }
    }

    async fn process_solution(&mut self, solution: work::Solution) {
        let job: &StratumJob = solution.job();

        let seq_num = self.seq_num;
        self.seq_num = self.seq_num.wrapping_add(1);

        let share_msg = SubmitSharesStandard {
            channel_id: job.channel_id,
            seq_num,
            job_id: job.id,
            nonce: solution.nonce(),
            ntime: solution.time(),
            version: solution.version(),
        };
        // store solution with sequence number for future server acknowledge
        self.client
            .solutions
            .lock()
            .await
            .push_back((solution, seq_num));
        // send solutions back to the stratum server
        self.connection_tx
            .send_msg(share_msg)
            .await
            .expect("Cannot send submit to stratum server");
        // the response is handled in a separate task
    }

    async fn run(mut self) -> job::SolutionReceiver {
        while let Some(solution) = self.solution_receiver.receive().await {
            self.process_solution(solution).await;
        }
        // Return back solution receiver after terminating
        self.solution_receiver
    }
}

struct StratumConnectionHandler {
    client: Arc<StratumClient>,
    init_target: ii_bitcoin::Target,
    status: Result<(), ()>,
}

impl StratumConnectionHandler {
    pub fn new(client: Arc<StratumClient>) -> Self {
        Self {
            client,
            init_target: Default::default(),
            status: Err(()),
        }
    }

    async fn setup_mining_connection(
        &mut self,
        connection: &mut Connection<Framing>,
    ) -> Result<(), ()> {
        let setup_msg = SetupConnection {
            protocol: 0,
            max_version: 2,
            min_version: 2,
            flags: 0,
            endpoint_host: Str0_255::from_string(self.client.connection_details.host.clone()),
            endpoint_port: self.client.connection_details.port,
            device: DeviceInfo {
                vendor: "Braiins".try_into()?,
                hw_rev: "1".try_into()?,
                fw_ver: "Braiins OS 2019-06-05".try_into()?,
                dev_id: "xyz".try_into()?,
            },
        };
        connection
            .send_msg(setup_msg)
            .await
            .expect("Cannot send stratum setup mining connection");
        let frame = connection
            .next()
            .await
            .expect("Cannot receive response for stratum setup mining connection")
            .unwrap();
        self.status = Err(());
        let response_msg = build_message_from_frame(frame)
            .expect("BUG: handle building setup connection response message");
        response_msg.accept(self).await;
        self.status
    }

    async fn open_channel(&mut self, connection: &mut Connection<Framing>) -> Result<(), ()> {
        let channel_msg = OpenStandardMiningChannel {
            req_id: 10,
            user: self.client.connection_details.user.clone().try_into()?,
            nominal_hashrate: 1e9,
            // Maximum bitcoin target is 0xffff << 208 (= difficulty 1 share)
            max_target: ii_bitcoin::Target::default().into(),
        };
        connection
            .send_msg(channel_msg)
            .await
            .expect("Cannot send stratum open channel");
        let frame = connection
            .next()
            .await
            .expect("Cannot receive response for stratum open channel")
            .unwrap();
        self.status = Err(());
        let response_msg = build_message_from_frame(frame)
            .expect("BUG: handle building open channel response message");
        response_msg.accept(self).await;
        self.status
    }

    async fn connect(mut self) -> Result<(Connection<Framing>, ii_bitcoin::Target), ()> {
        let socket_addr = self
            .client
            .connection_details
            .get_host_and_port()
            .to_socket_addrs()
            .expect("BUG: invalid server address")
            .next()
            .expect("BUG: cannot resolve any IP address");

        let mut connection = Connection::<Framing>::connect(&socket_addr)
            .await
            .expect("Cannot connect to stratum server");
        self.setup_mining_connection(&mut connection)
            .await
            .expect("Cannot setup stratum mining connection");
        self.open_channel(&mut connection)
            .await
            .expect("Cannot open stratum channel");

        Ok((connection, self.init_target))
    }
}

#[async_trait]
impl Handler for StratumConnectionHandler {
    async fn visit_setup_connection_success(
        &mut self,
        _msg: &Message<Protocol>,
        _success_msg: &SetupConnectionSuccess,
    ) {
        self.status = Ok(());
    }

    async fn visit_setup_connection_error(
        &mut self,
        _msg: &Message<Protocol>,
        _error_msg: &SetupConnectionError,
    ) {
        self.status = Err(());
    }

    async fn visit_open_standard_mining_channel_success(
        &mut self,
        _msg: &Message<Protocol>,
        success_msg: &OpenStandardMiningChannelSuccess,
    ) {
        self.init_target = success_msg.target.into();
        self.status = Ok(());
    }

    async fn visit_open_standard_mining_channel_error(
        &mut self,
        _msg: &Message<Protocol>,
        _error_msg: &OpenStandardMiningChannelError,
    ) {
        self.status = Err(());
    }
}

#[derive(Debug, ClientNode)]
pub struct StratumClient {
    connection_details: ConnectionDetails,
    #[member_client_stats]
    client_stats: stats::BasicClient,
    status: sync::AtomicStatus,
    last_job: Mutex<Option<Arc<StratumJob>>>,
    solutions: SolutionQueue,
    job_solver: Mutex<Option<job::Solver>>,
}

impl StratumClient {
    pub fn new(connection_details: ConnectionDetails, job_solver: job::Solver) -> Self {
        Self {
            connection_details,
            client_stats: Default::default(),
            status: sync::AtomicStatus::new(sync::Status::Created),
            last_job: Mutex::new(None),
            solutions: Mutex::new(VecDeque::new()),
            job_solver: Mutex::new(Some(job_solver)),
        }
    }

    async fn take_job_solver(&self) -> job::Solver {
        self.job_solver
            .lock()
            .await
            .take()
            .expect("BUG: missing job solver")
    }

    async fn return_job_solver(
        &self,
        job_sender: job::Sender,
        solution_receiver: job::SolutionReceiver,
    ) {
        let old = self.job_solver.lock().await.replace(job::Solver {
            job_sender,
            solution_receiver,
        });
        assert!(old.is_none(), "BUG: unexpected job solver");
    }

    async fn update_last_job(&self, job: Arc<StratumJob>) {
        self.last_job.lock().await.replace(job);
    }

    async fn run(self: Arc<Self>, solver: job::Solver) {
        let (connection, init_target) = StratumConnectionHandler::new(self.clone())
            .connect()
            .await
            .expect("Cannot initiate stratum connection");

        // FIXME: It must be set with `compare_and_swap`
        self.status.store(sync::Status::Running, Ordering::Relaxed);
        let (connection_rx, connection_tx) = connection.split();

        let (job_sender, solution_receiver) = join!(
            StratumEventHandler::new(self.clone(), connection_rx, solver.job_sender, init_target)
                .run(),
            StratumSolutionHandler::new(self.clone(), connection_tx, solver.solution_receiver)
                .run()
        );

        self.return_job_solver(job_sender, solution_receiver).await;
        // TODO: Implement `Restarting` state
        // TODO: Store `Failed` when some error occurred
        self.status.store(sync::Status::Stopped, Ordering::Relaxed);
    }
}

#[async_trait]
impl node::Client for StratumClient {
    async fn status(self: Arc<Self>) -> sync::Status {
        self.status.load(Ordering::Relaxed)
    }

    async fn start(self: Arc<Self>) {
        let mut status = self.status.load(Ordering::Relaxed);

        loop {
            let previous = status;
            match status {
                sync::Status::Created | sync::Status::Stopped | sync::Status::Failed => {
                    status = self.status.compare_and_swap(
                        status,
                        sync::Status::Starting,
                        Ordering::Relaxed,
                    );
                    if status == previous {
                        // The client can be safely run
                        let solver = self.take_job_solver().await;
                        tokio::spawn(self.clone().run(solver));
                        break;
                    }
                }
                sync::Status::Stopping | sync::Status::Failing => {
                    // Try to change state to `Restarting`
                    status = self.status.compare_and_swap(
                        status,
                        sync::Status::Restarting,
                        Ordering::Relaxed,
                    );
                    if status == previous {
                        break;
                    }
                }
                // Client is currently started
                sync::Status::Starting | sync::Status::Running | sync::Status::Restarting => break,
            };
            // Try it again because another task change the state
        }
    }

    async fn stop(self: Arc<Self>) {
        // TODO: send broadcast to disconnect client
    }

    async fn get_last_job(&self) -> Option<Arc<dyn job::Bitcoin>> {
        self.last_job
            .lock()
            .await
            .as_ref()
            .map(|job| job.clone() as Arc<dyn job::Bitcoin>)
    }
}

impl fmt::Display for StratumClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}@{}",
            client::Protocol::SCHEME_STRATUM_V2,
            self.connection_details.host,
            self.connection_details.user
        )
    }
}
