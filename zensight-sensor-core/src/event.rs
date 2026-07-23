//! Event publishing over the `events` class (#534).
//!
//! [`EventPublisher`] mirrors [`AlertReporter`](crate::AlertReporter)'s
//! ergonomics for the third data class: append-only records on
//! `v1/<origin>/events/<producer>/<subject...>/<id>`, published through the
//! shared declared-publisher registry with [`QosClass::Event`] (reliable +
//! block — a dropped event is unrecoverable, nothing supersedes it). The
//! record's ULID is the last key chunk, so records never overwrite and a
//! storage aligned on the events tree keeps an append-only log.

use zensight_common::{EventRecord, QosClass, encode};

use crate::error::{Result, SensorError};
use crate::publisher::Publisher;

/// Publishes [`EventRecord`]s under this producer's events tree.
pub struct EventPublisher {
    publisher: Publisher,
}

impl EventPublisher {
    /// Wrap the sensor's [`Publisher`] (from
    /// [`SensorRunner::publisher`](crate::SensorRunner::publisher)); events
    /// are encoded with the publisher's configured format.
    pub fn new(publisher: Publisher) -> Self {
        Self { publisher }
    }

    /// The full events key for `subject` + the record's id:
    /// `v1/<origin>/events/<producer>/<subject...>/<id>`.
    ///
    /// Subject chunks must be grammar-valid (lowercase alnum + `._-`);
    /// invalid chunks are an error, not a panic — a malformed device name
    /// must never kill a sensor loop.
    fn event_key(&self, subject: &[&str], id: &str) -> Result<String> {
        let ctx = self.publisher.v1();
        let mut chunks: Vec<&str> = subject.to_vec();
        chunks.push(id);
        zenkey::grammar::data_key(
            ctx.origin(),
            zenkey::grammar::Class::Events,
            Some(ctx.producer()),
            &chunks,
        )
        .map(Into::into)
        .map_err(|e| SensorError::Publish {
            key: format!("events/{}", subject.join("/")),
            message: e.to_string(),
        })
    }

    /// Publish one record under `events/<producer>/<subject...>/<record.id>`.
    pub async fn publish(&self, subject: &[&str], record: &EventRecord) -> Result<()> {
        let key = self.event_key(subject, &record.id)?;
        let payload = encode(record, self.publisher.format())
            .map_err(|e| SensorError::Serialization(e.to_string()))?;
        self.publisher
            .publish_raw(&key, payload, QosClass::Event)
            .await
    }
}
