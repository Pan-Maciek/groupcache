#![allow(clippy::items_after_test_module)]
mod common;
use anyhow::{Context, Result};
use common::*;
use metrics::{Counter, CounterFn, Gauge, Histogram, Key, KeyName, Recorder, SharedString, Unit};
use pretty_assertions::assert_eq;
use rstest::{fixture, rstest};
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[fixture]
#[once]
pub fn mock_recorder() -> &'static MockRecorder {
    let recorder_mock = &*Box::leak(Box::new(MockRecorder::new()));
    metrics::set_recorder(recorder_mock)
        .expect("setting metrics recorder should succeed as long as it's done once per process");
    recorder_mock
}

#[fixture]
pub fn clean_recorder(mock_recorder: &'static MockRecorder) -> &'static MockRecorder {
    mock_recorder.clean();
    return mock_recorder;
}

#[fixture]
pub async fn groupcache() -> TestGroupcache {
    single_instance()
        .await
        .expect("running single groupcache instance should succeed")
}

#[rstest]
#[case::get("groupcache.get_total")]
#[case::local_load("groupcache.local_load_total")]
#[tokio::test]
#[serial_test::serial]
pub async fn metrics_on_first_load_test(
    clean_recorder: &'static MockRecorder,
    #[future] groupcache: TestGroupcache,
    #[case] metric_name: &str,
) -> Result<()> {
    let _ = groupcache.await.get("foo").await?;

    let counter_value = clean_recorder
        .counter_value(metric_name)
        .context("should be set")?;

    assert_eq!(1, counter_value);
    Ok(())
}

#[tokio::test]
async fn test_foo() -> Result<()> {
    let (instance_one, instance_two) = two_connected_instances().await?;
    let key_on_instance_2 = error_key_on_instance(instance_two.clone());

    let err = instance_one
        .get(&key_on_instance_2)
        .await
        .expect_err("expected error from loader");

    assert_eq!(
        "Transport error: 'Loading error: 'Something bad happened during loading :/''",
        err.to_string()
    );

    Ok(())
}

pub struct MockRecorder {
    registered_counters: RefCell<HashMap<String, Arc<MockCounter>>>,
}

impl MockRecorder {
    fn new() -> Self {
        Self {
            registered_counters: RefCell::new(Default::default()),
        }
    }

    fn counter_value(&self, key: &str) -> Option<u64> {
        let counters = self.registered_counters.borrow();
        return counters.get(key).map(|c| {
            let guard = c.count.lock().unwrap();
            *guard
        });
    }

    fn clean(&self) {
        let mut counters = self.registered_counters.borrow_mut();
        counters.drain();
    }
}

#[derive(Clone)]
struct MockCounter {
    count: Arc<Mutex<u64>>,
}

impl MockCounter {
    fn new() -> Self {
        Self {
            count: Arc::new(Mutex::new(0)),
        }
    }
}

impl CounterFn for MockCounter {
    fn increment(&self, value: u64) {
        let mut guard = self.count.lock().unwrap();
        *guard += value;
    }

    fn absolute(&self, value: u64) {
        let mut guard = self.count.lock().unwrap();
        *guard = value;
    }
}

impl Recorder for MockRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn register_counter(&self, key: &Key) -> Counter {
        match self
            .registered_counters
            .borrow_mut()
            .entry(key.name().to_string())
        {
            Entry::Occupied(e) => {
                let mock_counter = e.get().clone();
                Counter::from(mock_counter)
            }
            Entry::Vacant(e) => {
                let mock_counter = Arc::new(MockCounter::new());
                e.insert(mock_counter.clone());

                Counter::from(mock_counter)
            }
        }
    }

    fn register_gauge(&self, _key: &Key) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, _key: &Key) -> Histogram {
        Histogram::noop()
    }
}
