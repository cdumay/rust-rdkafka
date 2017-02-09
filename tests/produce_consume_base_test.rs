extern crate rdkafka;
extern crate futures;
extern crate rand;

mod test_utils;

use futures::*;

use rdkafka::config::{ClientConfig, TopicConfig};
use rdkafka::consumer::{Consumer, CommitMode, EmptyConsumerContext};
use rdkafka::consumer::stream_consumer::StreamConsumer;
use rdkafka::producer::FutureProducer;
use rdkafka::topic_partition_list::TopicPartitionList;
use rdkafka::message::ToBytes;

use test_utils::{rand_test_group, rand_test_topic};

use std::collections::HashMap;

fn produce_messages<P, K, J, Q>(topic_name: &str, count: i32, value_fn: &P, key_fn: &K, partition: Option<i32>)
    -> HashMap<(i32, i64), i32>
    where P: Fn(i32) -> J,
          K: Fn(i32) -> Q,
          J: ToBytes,
          Q: ToBytes {
    // Produce some messages
    let producer = ClientConfig::new()
        .set("bootstrap.servers", "localhost:9092")
        .create::<FutureProducer>()
        .expect("Producer creation error");

    producer.start();

    let topic_config = TopicConfig::new()
        .set("produce.offset.report", "true")
        .set("message.timeout.ms", "5000")
        .finalize();

    let topic = producer.get_topic(&topic_name, &topic_config)
        .expect("Topic creation error");

    let futures = (0..count)
        .map(|id| {
            let future = topic.send_copy(partition, Some(&value_fn(id)), Some(&key_fn(id)))
                .expect("Production failed");
            (id, future)
        }).collect::<Vec<_>>();

    let mut message_map = HashMap::new();
    for (id, future) in futures {
        match future.wait() {
            Ok(report) => match report.result() {
                Err(e) => panic!("Delivery failed: {}", e),
                Ok((partition, offset)) => message_map.insert((partition, offset), id),
            },
            Err(e) => panic!("Waiting for future failed: {}", e)
        };
    }

    message_map
}

// Create consumer
fn create_stream_consumer(topic_name: &str) -> StreamConsumer<EmptyConsumerContext> {
    let mut consumer = ClientConfig::new()
        .set("group.id", &rand_test_group())
        .set("bootstrap.servers", "localhost:9092")
        .set("enable.partition.eof", "false")
        .set("session.timeout.ms", "6000")
        .set("enable.auto.commit", "false")
        .set_default_topic_config(
            TopicConfig::new()
                .set("auto.offset.reset", "earliest")
                .finalize()
        )
        .create::<StreamConsumer<_>>()
        .expect("Consumer creation failed");
    consumer.subscribe(&vec![topic_name]).unwrap();
    consumer
}

fn value_fn(id: i32) -> String {
    format!("Message {}", id)
}

fn key_fn(id: i32) -> String {
    format!("Key {}", id)
}

// All produced messages should be consumed.
#[test]
fn test_produce_consume_base() {
    let topic_name = rand_test_topic();
    let message_map = produce_messages(&topic_name, 100, &value_fn, &key_fn, None);
    let mut consumer = create_stream_consumer(&topic_name);

    let _consumer_future = consumer.start()
        .take(100)
        .for_each(|message| {
            match message {
                Ok(m) => {
                    let id = message_map.get(&(m.partition(), m.offset())).unwrap();
                    assert_eq!(m.payload_view::<str>().unwrap().unwrap(), value_fn(*id));
                    assert_eq!(m.key_view::<str>().unwrap().unwrap(), key_fn(*id));
                },
                e => panic!("Error receiving message: {:?}", e)
            };
            Ok(())
        })
        .wait();
}

// All messages should go to the same partition.
#[test]
fn test_produce_partition() {
    let topic_name = rand_test_topic();
    let message_map = produce_messages(&topic_name, 100, &value_fn, &key_fn, Some(0));

    let res = message_map.iter()
        .filter(|&(&(partition, _), _)| partition == 0)
        .count();

    assert_eq!(res, 100);
}

#[test]
fn test_metadata() {
    let topic_name = rand_test_topic();
    produce_messages(&topic_name, 1, &value_fn, &key_fn, Some(0));
    produce_messages(&topic_name, 1, &value_fn, &key_fn, Some(1));
    produce_messages(&topic_name, 1, &value_fn, &key_fn, Some(2));
    let consumer = create_stream_consumer(&topic_name);

    let metadata = consumer.fetch_metadata(5000).unwrap();

    let topic_metadata = metadata.topics().iter()
        .find(|m| m.name() == topic_name).unwrap();

    let mut ids = topic_metadata.partitions().iter().map(|p| p.id()).collect::<Vec<_>>();
    ids.sort();

    assert_eq!(ids, vec![0, 1, 2]);
    // assert_eq!(topic_metadata.error(), None);
    assert_eq!(topic_metadata.partitions().len(), 3);
    assert_eq!(topic_metadata.partitions()[0].leader(), 0);
    assert_eq!(topic_metadata.partitions()[1].leader(), 0);
    assert_eq!(topic_metadata.partitions()[2].leader(), 0);
    assert_eq!(topic_metadata.partitions()[0].replicas(), &[0]);
    assert_eq!(topic_metadata.partitions()[0].isr(), &[0]);
}

#[test]
fn test_consumer_commit() {
    let topic_name = rand_test_topic();
    produce_messages(&topic_name, 10, &value_fn, &key_fn, Some(0));
    produce_messages(&topic_name, 11, &value_fn, &key_fn, Some(1));
    produce_messages(&topic_name, 12, &value_fn, &key_fn, Some(2));
    let mut consumer = create_stream_consumer(&topic_name);


    let _consumer_future = consumer.start()
        .take(33)
        .for_each(|message| {
            match message {
                Ok(m) => {
                    if m.partition() == 1 {
                        consumer.commit_message(&m, CommitMode::Async).unwrap();
                    }
                },
                e => panic!("Error receiving message: {:?}", e)
            };
            Ok(())
        })
        .wait();

    assert_eq!(consumer.fetch_watermarks(&topic_name, 0, 5000).unwrap(), (0, 10));
    assert_eq!(consumer.fetch_watermarks(&topic_name, 1, 5000).unwrap(), (0, 11));
    assert_eq!(consumer.fetch_watermarks(&topic_name, 2, 5000).unwrap(), (0, 12));

    let mut assignment = TopicPartitionList::new();
    assignment.add_topic_with_partitions_and_offsets(&topic_name, &vec![(0, -1001), (1, -1001), (2, -1001)]);
    assert_eq!(assignment, consumer.assignment().unwrap());

    let mut committed = TopicPartitionList::new();
    committed.add_topic_with_partitions_and_offsets(&topic_name, &vec![(0, -1001), (1, 11), (2, -1001)]);
    assert_eq!(committed, consumer.committed(5000).unwrap());

    let mut position = TopicPartitionList::new();
    position.add_topic_with_partitions_and_offsets(&topic_name, &vec![(0, 10), (1, 11), (2, 12)]);
    assert_eq!(position, consumer.position().unwrap());
}

#[test]
fn test_subscription() {
    let topic_name = rand_test_topic();
    produce_messages(&topic_name, 10, &value_fn, &key_fn, None);
    let mut consumer = create_stream_consumer(&topic_name);

    let _consumer_future = consumer.start().take(10).wait();

    let subscription = TopicPartitionList::with_topics(vec![topic_name.as_str()].as_slice());
    assert_eq!(subscription, consumer.subscription().unwrap());
}
