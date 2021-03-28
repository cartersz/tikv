// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::fmt::{self, Display, Formatter};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::server::metrics::*;
use engine_traits::{KvEngine, CF_DEFAULT};
use raftstore::coprocessor::RegionInfoProvider;
use tikv_util::time::{Instant, UnixSecs};
use tikv_util::worker::{Runnable, RunnableWithTimer};

const COMPACT_FILES_SLEEP_TIME: u64 = 2; // 2s
const WAIT_METRICS_PULLED_TIME: u64 = 40; // 40s

#[derive(Debug)]
pub enum Task {
    UpdatePollInterval(Duration),
}

impl Display for Task {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Task::UpdatePollInterval(interval) => {
                write!(f, "[ttl checker] update poll interval to {:?}", interval)
            }
        }
    }
}

pub struct TTLChecker<E: KvEngine, R: RegionInfoProvider> {
    engine: E,
    region_info_provider: R,
    poll_interval: Duration,
}

impl<E: KvEngine, R: RegionInfoProvider> TTLChecker<E, R> {
    pub fn new(engine: E, region_info_provider: R, poll_interval: Duration) -> Self {
        TTL_CHECKER_POLL_INTERVAL_GAUGE.set(poll_interval.as_millis() as i64);
        TTLChecker::<E, R> {
            engine,
            region_info_provider,
            poll_interval,
        }
    }
}

impl<E: KvEngine, R: RegionInfoProvider> Runnable for TTLChecker<E, R>
where
    E: KvEngine,
{
    type Task = Task;

    fn run(&mut self, task: Task) {
        match task {
            Task::UpdatePollInterval(interval) => {
                self.poll_interval = interval;
                info!(
                    "ttl checker poll interval is changed to {}s, will be take effect after next round",
                    interval.as_secs()
                );
                TTL_CHECKER_POLL_INTERVAL_GAUGE.set(self.poll_interval.as_millis() as i64);
            }
        }
    }
}

impl<E: KvEngine, R: RegionInfoProvider> RunnableWithTimer for TTLChecker<E, R> {
    fn on_timeout(&mut self) {
        let mut key = vec![];
        loop {
            let (tx, rx) = mpsc::channel();
            if let Err(e) = self.region_info_provider.seek_region(
                &key,
                Box::new(move |iter| {
                    let mut scanned_regions = 0;
                    let mut start_key = None;
                    let mut end_key = None;
                    for info in iter {
                        if start_key.is_none() {
                            start_key = Some(info.region.get_start_key().to_owned());
                        }
                        TTL_CHECKER_PROCESSED_REGIONS_GAUGE.inc();
                        scanned_regions += 1;
                        end_key = Some(info.region.get_end_key().to_vec());
                        if scanned_regions == 10 {
                            break;
                        }
                    }
                    if scanned_regions != 0 {
                        let _ = tx.send(Some((start_key.unwrap(), end_key.unwrap())));
                    } else {
                        let _ = tx.send(None);
                    }
                }),
            ) {
                error!(?e; "ttl checker: failed to get next region information");
                TTL_CHECKER_ACTIONS_COUNTER_VEC
                    .with_label_values(&["error"])
                    .inc();
                continue;
            }

            match rx.recv() {
                Ok(None) => {}
                Ok(Some((start_key, end_key))) => {
                    let start = keys::data_key(&start_key);
                    let end = keys::data_end_key(&end_key);
                    check_ttl_and_compact_files(&self.engine, &start, &end, true);
                    if !end_key.is_empty() {
                        key = end_key;
                        continue;
                    }
                }
                Err(e) => {
                    error!("ttl checker: failed to get next region information";
                        "err" => ?e);
                    TTL_CHECKER_ACTIONS_COUNTER_VEC
                        .with_label_values(&["error"])
                        .inc();
                    continue;
                }
            }
            break;
        }
        TTL_CHECKER_ACTIONS_COUNTER_VEC
            .with_label_values(&["finish"])
            .inc();
        info!(
            "ttl checker finishes a round, wait {}s to start next round",
            self.poll_interval.as_secs()
        );
        // make sure the data point of metrics is pulled
        thread::sleep(Duration::from_secs(WAIT_METRICS_PULLED_TIME));
        TTL_CHECKER_PROCESSED_REGIONS_GAUGE.set(0);
    }

    fn get_interval(&self) -> Duration {
        self.poll_interval
    }
}

fn check_ttl_and_compact_files<E: KvEngine>(
    engine: &E,
    start_key: &[u8],
    end_key: &[u8],
    exclude_l0: bool,
) {
    let current_ts = UnixSecs::now().into_inner();
    let mut files = Vec::new();
    let res = match engine.get_range_ttl_properties_cf(CF_DEFAULT, start_key, end_key) {
        Ok(v) => v,
        Err(e) => {
            error!(
                "get range ttl properties failed";
                "range_start" => log_wrappers::Value::key(&start_key),
                "range_end" => log_wrappers::Value::key(&end_key),
                "err" => %e,
            );
            TTL_CHECKER_ACTIONS_COUNTER_VEC
                .with_label_values(&["error"])
                .inc();
            return;
        }
    };
    if res.is_empty() {
        TTL_CHECKER_ACTIONS_COUNTER_VEC
            .with_label_values(&["empty"])
            .inc();
        return;
    }
    for (file_name, prop) in res {
        if prop.max_expire_ts <= current_ts {
            files.push(file_name);
        }
    }
    if files.is_empty() {
        TTL_CHECKER_ACTIONS_COUNTER_VEC
            .with_label_values(&["skip"])
            .inc();
        return;
    }

    let timer = Instant::now();
    let files_count = files.len();
    for file in files {
        let compact_range_timer = TTL_CHECKER_COMPACT_DURATION_HISTOGRAM.start_coarse_timer();
        if let Err(e) = engine.compact_files_cf(CF_DEFAULT, vec![file], None, 0, exclude_l0) {
            error!(
                "execute ttl compact files failed";
                "range_start" => log_wrappers::Value::key(&start_key),
                "range_end" => log_wrappers::Value::key(&end_key),
                "err" => %e,
            );
            TTL_CHECKER_ACTIONS_COUNTER_VEC
                .with_label_values(&["error"])
                .inc();
            continue;
        }
        compact_range_timer.observe_duration();
        TTL_CHECKER_ACTIONS_COUNTER_VEC
            .with_label_values(&["compact"])
            .inc();
        thread::sleep(Duration::from_secs(COMPACT_FILES_SLEEP_TIME));
    }

    debug!(
        "compact files finished";
        "files_count" => files_count,
        "time_takes" => ?timer.elapsed(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::DbConfig;
    use crate::storage::kv::TestEngineBuilder;
    use crate::storage::raw::ttl::TEST_CURRENT_TS;
    use engine_traits::util::append_expire_ts;
    use engine_traits::{MiscExt, Peekable, SyncMutable, CF_DEFAULT};

    #[test]
    fn test_ttl_checker() {
        let mut cfg = DbConfig::default();
        cfg.defaultcf.disable_auto_compactions = true;
        let dir = tempfile::TempDir::new().unwrap();
        let builder = TestEngineBuilder::new().path(dir.path()).ttl(true);
        let engine = builder.build_with_cfg(&cfg).unwrap();

        let kvdb = engine.get_rocksdb();
        let key1 = b"zkey1";
        let mut value1 = vec![0; 10];
        append_expire_ts(&mut value1, 10);
        kvdb.put_cf(CF_DEFAULT, key1, &value1).unwrap();
        kvdb.flush_cf(CF_DEFAULT, true).unwrap();
        let key2 = b"zkey2";
        let mut value2 = vec![0; 10];
        append_expire_ts(&mut value2, TEST_CURRENT_TS + 20);
        kvdb.put_cf(CF_DEFAULT, key2, &value2).unwrap();
        let key3 = b"zkey3";
        let mut value3 = vec![0; 10];
        append_expire_ts(&mut value3, 20);
        kvdb.put_cf(CF_DEFAULT, key3, &value3).unwrap();
        kvdb.flush_cf(CF_DEFAULT, true).unwrap();
        let key4 = b"zkey4";
        let mut value4 = vec![0; 10];
        append_expire_ts(&mut value4, 0);
        kvdb.put_cf(CF_DEFAULT, key4, &value4).unwrap();
        kvdb.flush_cf(CF_DEFAULT, true).unwrap();
        let key5 = b"zkey5";
        let mut value5 = vec![0; 10];
        append_expire_ts(&mut value5, 10);
        kvdb.put_cf(CF_DEFAULT, key5, &value5).unwrap();
        kvdb.flush_cf(CF_DEFAULT, true).unwrap();

        assert!(kvdb.get_value_cf(CF_DEFAULT, key1).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key2).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key3).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key4).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key5).unwrap().is_some());

        let _ = check_ttl_and_compact_files(&kvdb, b"zkey1", b"zkey25", false);
        assert!(kvdb.get_value_cf(CF_DEFAULT, key1).unwrap().is_none());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key2).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key3).unwrap().is_none());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key4).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key5).unwrap().is_some());

        let _ = check_ttl_and_compact_files(&kvdb, b"zkey2", b"zkey6", false);
        assert!(kvdb.get_value_cf(CF_DEFAULT, key1).unwrap().is_none());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key2).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key3).unwrap().is_none());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key4).unwrap().is_some());
        assert!(kvdb.get_value_cf(CF_DEFAULT, key5).unwrap().is_none());
    }
}