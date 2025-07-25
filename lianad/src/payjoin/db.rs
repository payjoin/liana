use payjoin::bitcoin;
use payjoin::persist::SessionPersister;
use payjoin::receive::v2::SessionEvent as ReceiverSessionEvent;
use payjoin::send::v2::SessionEvent as SenderSessionEvent;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;

use crate::database::DatabaseInterface;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionId(pub i64);

impl SessionId {
    pub fn new(id: i64) -> Self {
        Self(id)
    }
}

#[derive(Debug)]
pub enum PersisterError {
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    NotFound(String),
}

impl Display for PersisterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            PersisterError::Serialize(e) => write!(f, "Serialization failed: {e}"),
            PersisterError::Deserialize(e) => write!(f, "Deserialization failed: {e}"),
            PersisterError::NotFound(key) => write!(f, "Key not found: {key}"),
        }
    }
}

impl std::error::Error for PersisterError {}

#[derive(Clone)]
pub struct ReceiverPersister {
    db: Arc<dyn DatabaseInterface>,
    pub session_id: SessionId,
}

impl ReceiverPersister {
    pub fn new(db: Arc<dyn DatabaseInterface>) -> Self {
        let mut db_conn = db.connection();
        let session_id = db_conn.save_new_payjoin_receiver_session();
        Self {
            db,
            session_id: SessionId(session_id),
        }
    }

    pub fn from_id(db: Arc<dyn DatabaseInterface>, id: SessionId) -> Self {
        Self { db, session_id: id }
    }
}

impl SessionPersister for ReceiverPersister {
    type SessionEvent = ReceiverSessionEvent;
    type InternalStorageError = PersisterError;

    fn save_event(
        &self,
        event: &Self::SessionEvent,
    ) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        // serilize event
        let event_ser = serde_json::to_vec(event).map_err(PersisterError::Serialize)?;
        db_conn.save_receiver_session_event(&self.session_id, event_ser);
        Ok(())
    }

    fn load(
        &self,
    ) -> std::result::Result<Box<dyn Iterator<Item = Self::SessionEvent>>, Self::InternalStorageError>
    {
        let mut db_conn = self.db.connection();
        let events = db_conn.load_receiver_session_events(&self.session_id);
        let deserialized_events: Result<Vec<_>, _> = events
            .into_iter()
            .map(|event| serde_json::from_slice(&event).map_err(PersisterError::Deserialize))
            .collect();
        Ok(Box::new(deserialized_events?.into_iter()))
    }

    fn close(&self) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        db_conn.update_receiver_session_completed_at(&self.session_id);
        Ok(())
    }
}

#[derive(Clone)]
pub struct SenderPersister {
    db: Arc<dyn DatabaseInterface>,
    pub session_id: SessionId,
}

impl SenderPersister {
    pub fn new(db: Arc<dyn DatabaseInterface>, original_txid: &bitcoin::Txid) -> Self {
        let mut db_conn = db.connection();
        let session_id = db_conn.save_new_payjoin_sender_session(original_txid);
        Self {
            db,
            session_id: SessionId(session_id),
        }
    }

    pub fn from_id(db: Arc<dyn DatabaseInterface>, id: SessionId) -> Self {
        Self { db, session_id: id }
    }
}

impl SessionPersister for SenderPersister {
    type SessionEvent = SenderSessionEvent;
    type InternalStorageError = PersisterError;

    fn save_event(
        &self,
        event: &Self::SessionEvent,
    ) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        // serilize event
        let event_ser = serde_json::to_vec(event).map_err(PersisterError::Serialize)?;
        db_conn.save_sender_session_event(&self.session_id, event_ser);
        Ok(())
    }

    fn load(
        &self,
    ) -> std::result::Result<Box<dyn Iterator<Item = Self::SessionEvent>>, Self::InternalStorageError>
    {
        let mut db_conn = self.db.connection();
        let events = db_conn.get_all_sender_session_events(&self.session_id);
        let deserialized_events: Result<Vec<_>, _> = events
            .into_iter()
            .map(|event| serde_json::from_slice(&event).map_err(PersisterError::Deserialize))
            .collect();
        Ok(Box::new(deserialized_events?.into_iter()))
    }

    fn close(&self) -> std::result::Result<(), Self::InternalStorageError> {
        let mut db_conn = self.db.connection();
        db_conn.update_sender_session_completed_at(&self.session_id);
        Ok(())
    }
}
