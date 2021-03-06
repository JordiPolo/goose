use lazy_static::lazy_static;
use nng::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::{thread, time};

use crate::goose::GooseRequest;
use crate::stats;
use crate::util;
use crate::{GooseAttack, GooseConfiguration, GooseUserCommand};

/// How long the manager will wait for all workers to stop after the load test ends.
const GRACEFUL_SHUTDOWN_TIMEOUT: usize = 30;

/// All elements required to initialize a user in a worker process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GooseUserInitializer {
    /// An index into the internal `GooseTest.task_sets` vector, indicating which GooseTaskSet is running.
    pub task_sets_index: usize,
    /// The base_url for this user thread.
    pub base_url: String,
    /// Minimum amount of time to sleep after running a task.
    pub min_wait: usize,
    /// Maximum amount of time to sleep after running a task.
    pub max_wait: usize,
    /// A local copy of the global GooseConfiguration.
    pub config: GooseConfiguration,
    /// Numerical identifier for worker.
    pub worker_id: usize,
}

// Mutable singleton globally tracking how many workers are currently being managed.
lazy_static! {
    static ref ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
}

fn distribute_users(goose_attack: &GooseAttack) -> (usize, usize) {
    let users_per_worker =
        goose_attack.users / (goose_attack.configuration.expect_workers as usize);
    let users_remainder = goose_attack.users % (goose_attack.configuration.expect_workers as usize);
    if users_remainder > 0 {
        info!(
            "each worker to start {} users, assigning 1 extra to {} workers",
            users_per_worker, users_remainder
        );
    } else {
        info!("each worker to start {} users", users_per_worker);
    }
    (users_per_worker, users_remainder)
}

fn pipe_closed(_pipe: Pipe, event: PipeEvent) {
    match event {
        PipeEvent::AddPost => {
            debug!("worker pipe added");
            ACTIVE_WORKERS.fetch_add(1, Ordering::SeqCst);
        }
        PipeEvent::RemovePost => {
            let active_workers = ACTIVE_WORKERS.fetch_sub(1, Ordering::SeqCst);
            info!("worker {} exited", active_workers);
        }
        _ => {}
    }
}

/// Merge per-user-statistics from user thread into global parent statistics
fn merge_from_worker(
    parent_request: &GooseRequest,
    user_request: &GooseRequest,
    config: &GooseConfiguration,
) -> GooseRequest {
    // Make a mutable copy where we can merge things
    let mut merged_request = parent_request.clone();
    // Iterate over user response times, and merge into global response time
    merged_request.response_times = stats::merge_response_times(
        merged_request.response_times,
        user_request.response_times.clone(),
    );
    // Increment total response time counter.
    merged_request.total_response_time += &user_request.total_response_time;
    // Increment count of how many response counters we've seen.
    merged_request.response_time_counter += &user_request.response_time_counter;
    // If user had new fastest response time, update global fastest response time.
    merged_request.min_response_time = stats::update_min_response_time(
        merged_request.min_response_time,
        user_request.min_response_time,
    );
    // If user had new slowest response time, update global slowest resposne time.
    merged_request.max_response_time = stats::update_max_response_time(
        merged_request.max_response_time,
        user_request.max_response_time,
    );
    // Increment total success counter.
    merged_request.success_count += &user_request.success_count;
    // Increment total fail counter.
    merged_request.fail_count += &user_request.fail_count;
    // Only accrue overhead of merging status_code_counts if we're going to display the results
    if config.status_codes {
        for (status_code, count) in &user_request.status_code_counts {
            let new_count;
            // Add user count into global count
            if let Some(existing_status_code_count) =
                merged_request.status_code_counts.get(&status_code)
            {
                new_count = *existing_status_code_count + *count;
            }
            // No global count exists yet, so start with user count
            else {
                new_count = *count;
            }
            merged_request
                .status_code_counts
                .insert(*status_code, new_count);
        }
    }
    merged_request
}

pub async fn manager_main(mut goose_attack: GooseAttack) -> GooseAttack {
    // Creates a TCP address.
    let address = format!(
        "tcp://{}:{}",
        goose_attack.configuration.manager_bind_host, goose_attack.configuration.manager_bind_port
    );
    info!("worker connecting to manager at {}", &address);

    // Create a Rep0 reply socket.
    let server = match Socket::new(Protocol::Rep0) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to create socket: {}.", e);
            std::process::exit(1);
        }
    };

    // Set up callback function to receive pipe event notifications.
    match server.pipe_notify(pipe_closed) {
        Ok(_) => (),
        Err(e) => {
            error!("failed to set up pipe handler: {}", e);
            std::process::exit(1);
        }
    }

    // Listen for connections.
    match server.listen(&address) {
        Ok(s) => (s),
        Err(e) => {
            error!("failed to bind to socket {}: {}.", address, e);
            std::process::exit(1);
        }
    }
    info!(
        "manager listening on {}, waiting for {} workers",
        &address, goose_attack.configuration.expect_workers
    );

    // Calculate how many users each worker will be responsible for.
    let (users_per_worker, mut users_remainder) = distribute_users(&goose_attack);

    // A mutable bucket of users to be assigned to workers.
    let mut available_users = goose_attack.weighted_users.clone();

    // Track how many workers we've seen.
    let mut workers: HashSet<Pipe> = HashSet::new();

    // Track start time, we'll reset this when the test actually starts.
    let mut started = time::Instant::now();
    let mut running_statistics_timer = time::Instant::now();
    let mut exit_timer = time::Instant::now();
    let mut load_test_running = false;
    let mut load_test_finished = false;

    // Catch ctrl-c to allow clean shutdown to display statistics.
    let canceled = Arc::new(AtomicBool::new(false));
    util::setup_ctrlc_handler(&canceled);

    // Worker control loop.
    loop {
        // While running load test, check if any workers go away.
        if !load_test_finished {
            // If ACTIVE_WORKERS is less than the total workers seen, a worker went away.
            if ACTIVE_WORKERS.load(Ordering::SeqCst) < workers.len() {
                // If worked goes away during load test, exit gracefully.
                if load_test_running {
                    info!(
                        "worker went away, stopping gracefully afer {} seconds...",
                        started.elapsed().as_secs()
                    );
                    load_test_finished = true;
                    exit_timer = time::Instant::now();
                }
                // If a worker goes away during start up, exit immediately.
                else {
                    warn!("worker went away, stopping immediately...");
                    break;
                }
            }
        }
        if load_test_running {
            if !load_test_finished {
                // Test ran to completion or was canceled with ctrl-c.
                if util::timer_expired(started, goose_attack.run_time)
                    || canceled.load(Ordering::SeqCst)
                {
                    info!("stopping after {} seconds...", started.elapsed().as_secs());
                    load_test_finished = true;
                    exit_timer = time::Instant::now();
                }
            }

            // Aborting graceful shutdown, workers took too long to shut down.
            if load_test_finished && util::timer_expired(exit_timer, GRACEFUL_SHUTDOWN_TIMEOUT) {
                warn!("graceful shutdown timer expired, exiting...");
                break;
            }

            // When displaying running statistics, sync data from user threads first.
            if !goose_attack.configuration.only_summary
                && util::timer_expired(running_statistics_timer, crate::RUNNING_STATS_EVERY)
            {
                // Reset timer each time we display statistics.
                running_statistics_timer = time::Instant::now();
                stats::print_running_stats(&goose_attack, started.elapsed().as_secs() as usize);
            }
        } else if canceled.load(Ordering::SeqCst) {
            info!("load test canceled, exiting");
            std::process::exit(1);
        }

        // Check for messages from workers.
        match server.try_recv() {
            Ok(mut msg) => {
                // Message received, grab the pipe to determine which worker it is.
                let pipe = match msg.pipe() {
                    Some(p) => p,
                    None => {
                        error!("unexpected fatal error reading worker pipe");
                        std::process::exit(1);
                    }
                };

                // Workers always send a HashMap<String, GooseRequest>.
                let requests: HashMap<String, GooseRequest> =
                    serde_cbor::from_reader(msg.as_slice()).unwrap();
                debug!("requests statistics received: {:?}", requests.len());

                // If workers already contains this pipe, we've seen this worker before.
                if workers.contains(&pipe) {
                    let mut message = Message::new().unwrap();
                    // All workers are running load test, sending statistics.
                    if workers.len() == goose_attack.configuration.expect_workers as usize {
                        // Requests statistics received, merge them into our local copy.
                        if requests.len() > 0 {
                            debug!("requests statistics received: {:?}", requests.len());
                            for (request_key, request) in requests {
                                trace!("request_key: {}", request_key);
                                let merged_request;
                                if let Some(parent_request) =
                                    goose_attack.merged_requests.get(&request_key)
                                {
                                    merged_request = merge_from_worker(
                                        parent_request,
                                        &request,
                                        &goose_attack.configuration,
                                    );
                                } else {
                                    // First time seeing this request, simply insert it.
                                    merged_request = request.clone();
                                }
                                goose_attack
                                    .merged_requests
                                    .insert(request_key.to_string(), merged_request);
                            }
                        }
                        // Notify the worker that the load test is over and to exit.
                        if load_test_finished {
                            debug!("telling worker to exit");
                            match serde_cbor::to_writer(&mut message, &GooseUserCommand::EXIT) {
                                Ok(_) => (),
                                Err(e) => {
                                    error!("failed to serialize user command: {}", e);
                                    std::process::exit(1);
                                }
                            }
                        }
                        // Notify the worker that the load test is still running.
                        else {
                            match serde_cbor::to_writer(&mut message, &GooseUserCommand::RUN) {
                                Ok(_) => (),
                                Err(e) => {
                                    error!("failed to serialize user command: {}", e);
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
                    // All workers are not yet running, tell worker to wait.
                    else {
                        match serde_cbor::to_writer(&mut message, &GooseUserCommand::WAIT) {
                            Ok(_) => (),
                            Err(e) => {
                                error!("failed to serialize user command: {}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                    match server.try_send(message) {
                        Ok(_) => (),
                        // Determine why there was an error.
                        Err((_, e)) => {
                            match e {
                                // A worker went away, this happens during shutdown.
                                Error::TryAgain => {
                                    if ACTIVE_WORKERS.load(Ordering::SeqCst) == 0 {
                                        info!("all workers have exited");
                                        break;
                                    }
                                }
                                // An unexpected error.
                                _ => {
                                    error!("communication failure: {:?}", e);
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
                }
                // This is the first time we've seen this worker.
                else {
                    // Make sure we're not already connected to all of our workers.
                    if workers.len() >= goose_attack.configuration.expect_workers as usize {
                        // We already have enough workers, tell this extra one to EXIT.
                        let mut message = Message::new().unwrap();
                        match serde_cbor::to_writer(&mut message, &GooseUserCommand::EXIT) {
                            Ok(_) => (),
                            Err(e) => {
                                error!("failed to serialize user command: {}", e);
                                std::process::exit(1);
                            }
                        }
                        match server.try_send(message) {
                            Ok(_) => (),
                            // Determine why our send failed.
                            Err((_, e)) => match e {
                                Error::TryAgain => {
                                    if ACTIVE_WORKERS.load(Ordering::SeqCst) == 0 {
                                        info!("all workers have exited");
                                        break;
                                    }
                                }
                                _ => {
                                    error!("communication failure: {:?}", e);
                                    std::process::exit(1);
                                }
                            },
                        }
                    }
                    // We need another worker, accept the connection.
                    else {
                        // Validate worker load test hash.
                        match requests.get("load_test_hash") {
                            Some(r) => {
                                if r.load_test_hash != goose_attack.task_sets_hash {
                                    if goose_attack.configuration.no_hash_check {
                                        warn!("worker is running a different load test, ignoring")
                                    } else {
                                        error!("worker is running a different load test, set --no-hash-check to ignore");
                                        std::process::exit(1);
                                    }
                                }
                            }
                            None => {
                                if goose_attack.configuration.no_hash_check {
                                    warn!("worker is running a different load test, ignoring")
                                } else {
                                    error!("worker is running a different load test, set --no-hash-check to ignore");
                                    std::process::exit(1);
                                }
                            }
                        };

                        workers.insert(pipe);
                        info!(
                            "worker {} of {} connected",
                            workers.len(),
                            goose_attack.configuration.expect_workers
                        );

                        // Send new worker a batch of users.
                        let mut user_batch = users_per_worker;
                        // If remainder, put extra user in this batch.
                        if users_remainder > 0 {
                            users_remainder -= 1;
                            user_batch += 1;
                        }
                        let mut users = Vec::new();

                        // Pop users from available_users vector and build worker initializer.
                        for _ in 1..=user_batch {
                            let user = match available_users.pop() {
                                Some(u) => u,
                                None => {
                                    error!("not enough available users!?");
                                    std::process::exit(1);
                                }
                            };
                            // Build a vector of GooseUser initializers for next worker.
                            users.push(GooseUserInitializer {
                                task_sets_index: user.task_sets_index,
                                base_url: user.base_url.read().await.to_string(),
                                min_wait: user.min_wait,
                                max_wait: user.max_wait,
                                config: user.config.clone(),
                                worker_id: workers.len(),
                            });
                        }

                        // Send vector of user initializers to worker.
                        let mut message = Message::new().unwrap();
                        match serde_cbor::to_writer(&mut message, &users) {
                            Ok(_) => (),
                            Err(e) => {
                                error!("failed to serialize user initializers: {}", e);
                                std::process::exit(1);
                            }
                        }
                        info!("sending {} users to worker {}", users.len(), workers.len());
                        match server.try_send(message) {
                            Ok(_) => (),
                            Err((_, e)) => match e {
                                Error::TryAgain => {
                                    if ACTIVE_WORKERS.load(Ordering::SeqCst) == 0 {
                                        info!("all workers have exited");
                                        break;
                                    }
                                }
                                _ => {
                                    error!("communication failure: {:?}", e);
                                    std::process::exit(1);
                                }
                            },
                        }

                        if workers.len() == goose_attack.configuration.expect_workers as usize {
                            info!("gaggle distributed load test started");
                            // Reset start time, the distributed load test is truly starting now.
                            started = time::Instant::now();
                            running_statistics_timer = time::Instant::now();
                            load_test_running = true;
                        }
                    }
                }
            }
            Err(e) => {
                if e == Error::TryAgain {
                    if workers.len() > 0 {
                        if ACTIVE_WORKERS.load(Ordering::SeqCst) == 0 {
                            info!("all workers have exited");
                            break;
                        }
                    }
                    if !load_test_finished {
                        // Sleep half a second then return to the loop.
                        thread::sleep(time::Duration::from_millis(500));
                    }
                } else {
                    error!("unexpected error receiving user message: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
    goose_attack
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distribute_users() {
        let config = GooseConfiguration::default();
        let mut goose_attack = GooseAttack::initialize_with_config(config);

        goose_attack.users = 10;
        goose_attack.configuration.expect_workers = 2;
        let (users_per_process, users_remainder) = distribute_users(&goose_attack);
        assert_eq!(users_per_process, 5);
        assert_eq!(users_remainder, 0);

        goose_attack.users = 1;
        goose_attack.configuration.expect_workers = 1;
        let (users_per_process, users_remainder) = distribute_users(&goose_attack);
        assert_eq!(users_per_process, 1);
        assert_eq!(users_remainder, 0);

        goose_attack.users = 100;
        goose_attack.configuration.expect_workers = 21;
        let (users_per_process, users_remainder) = distribute_users(&goose_attack);
        assert_eq!(users_per_process, 4);
        assert_eq!(users_remainder, 16);
    }
}
