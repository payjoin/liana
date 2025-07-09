use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PayjoinStatus {
    Pending = 0,
    WaitingReceiver = 1,
    Signing = 2,
    Completed = 3,
    // TODO: more specific enums for why it failed
    Failed = 4,
}

impl From<i32> for PayjoinStatus {
    fn from(status: i32) -> Self {
        match status {
            0 => PayjoinStatus::Pending,
            1 => PayjoinStatus::WaitingReceiver,
            2 => PayjoinStatus::Signing,
            3 => PayjoinStatus::Completed,
            4 => PayjoinStatus::Failed,
            _ => panic!("Invalid payjoin status: {}", status),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PayjoinInfo {
    pub status: Option<PayjoinStatus>,
    pub bip21: String,
}
