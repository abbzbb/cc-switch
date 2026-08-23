use super::DeepLinkImportRequest;
use crate::error::AppError;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

const MAX_PENDING_ITEMS: usize = 128;
const MAX_PENDING_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepLinkErrorPayload {
    pub url: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum DeepLinkInboxPayload {
    Import(Box<DeepLinkImportRequest>),
    Error(DeepLinkErrorPayload),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeepLinkInboxItem {
    pub id: String,
    #[serde(flatten)]
    pub payload: DeepLinkInboxPayload,
}

#[derive(Default)]
struct DeepLinkInbox {
    next_id: u64,
    listener_ready: bool,
    pending_payload_bytes: usize,
    pending: VecDeque<QueuedInboxItem>,
}

struct QueuedInboxItem {
    item: DeepLinkInboxItem,
    payload_bytes: usize,
}

impl DeepLinkInbox {
    fn enqueue(
        &mut self,
        payload: DeepLinkInboxPayload,
    ) -> Result<(DeepLinkInboxItem, bool), AppError> {
        if self.pending.len() >= MAX_PENDING_ITEMS {
            return Err(AppError::Message(format!(
                "Deep link inbox is full (maximum {MAX_PENDING_ITEMS} pending items)"
            )));
        }
        let payload_bytes = serde_json::to_vec(&payload)
            .map_err(|error| {
                AppError::Message(format!("Failed to size deep link payload: {error}"))
            })?
            .len();
        let pending_payload_bytes = self
            .pending_payload_bytes
            .checked_add(payload_bytes)
            .ok_or_else(|| AppError::Message("Deep link inbox byte count overflow".to_string()))?;
        if pending_payload_bytes > MAX_PENDING_PAYLOAD_BYTES {
            return Err(AppError::Message(format!(
                "Deep link inbox payload budget exceeded (maximum {MAX_PENDING_PAYLOAD_BYTES} bytes)"
            )));
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| AppError::Message("Deep link inbox ID space exhausted".to_string()))?;
        let item = DeepLinkInboxItem {
            id: self.next_id.to_string(),
            payload,
        };
        self.pending.push_back(QueuedInboxItem {
            item: item.clone(),
            payload_bytes,
        });
        self.pending_payload_bytes = pending_payload_bytes;
        Ok((item, self.listener_ready))
    }

    fn mark_listener_ready(&mut self) -> Vec<DeepLinkInboxItem> {
        self.listener_ready = true;
        self.pending
            .iter()
            .map(|queued| queued.item.clone())
            .collect()
    }

    fn ack(&mut self, id: &str) -> bool {
        let Some(index) = self.pending.iter().position(|queued| queued.item.id == id) else {
            return false;
        };
        let removed = self.pending.remove(index).expect("located pending item");
        self.pending_payload_bytes = self
            .pending_payload_bytes
            .checked_sub(removed.payload_bytes)
            .expect("pending payload accounting underflow");
        true
    }
}

static DEEPLINK_INBOX: LazyLock<Mutex<DeepLinkInbox>> =
    LazyLock::new(|| Mutex::new(DeepLinkInbox::default()));

fn with_inbox<T>(operation: impl FnOnce(&mut DeepLinkInbox) -> T) -> T {
    let mut inbox = DEEPLINK_INBOX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    operation(&mut inbox)
}

pub fn enqueue_import(
    request: DeepLinkImportRequest,
) -> Result<(DeepLinkInboxItem, bool), AppError> {
    with_inbox(|inbox| inbox.enqueue(DeepLinkInboxPayload::Import(Box::new(request))))
}

pub fn enqueue_error(url: String, error: String) -> Result<(DeepLinkInboxItem, bool), AppError> {
    with_inbox(|inbox| {
        inbox.enqueue(DeepLinkInboxPayload::Error(DeepLinkErrorPayload {
            url,
            error,
        }))
    })
}

pub fn mark_listener_ready() -> Vec<DeepLinkInboxItem> {
    with_inbox(DeepLinkInbox::mark_listener_ready)
}

pub fn ack(id: &str) -> bool {
    with_inbox(|inbox| inbox.ack(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str) -> DeepLinkImportRequest {
        DeepLinkImportRequest {
            version: "v1".to_string(),
            resource: "provider".to_string(),
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn queues_before_ready_and_preserves_fifo_order() {
        let mut inbox = DeepLinkInbox::default();
        let (first, should_emit_first) = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("first"))))
            .unwrap();
        let (second, should_emit_second) = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("second"))))
            .unwrap();

        assert!(!should_emit_first);
        assert!(!should_emit_second);
        assert_eq!(
            inbox
                .mark_listener_ready()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec![first.id.as_str(), second.id.as_str()]
        );
    }

    #[test]
    fn ack_removes_only_the_matching_item() {
        let mut inbox = DeepLinkInbox::default();
        let (first, _) = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("first"))))
            .unwrap();
        let (second, _) = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("second"))))
            .unwrap();

        assert!(inbox.ack(&first.id));
        assert!(!inbox.ack("missing"));
        assert_eq!(
            inbox
                .mark_listener_ready()
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec![second.id.as_str()]
        );
    }

    #[test]
    fn new_items_are_emittable_after_listener_ready_but_remain_pending() {
        let mut inbox = DeepLinkInbox::default();
        assert!(inbox.mark_listener_ready().is_empty());

        let (item, should_emit) = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("later"))))
            .unwrap();

        assert!(should_emit);
        assert_eq!(inbox.mark_listener_ready()[0].id, item.id);
    }

    #[test]
    fn serializes_the_frontend_envelope_contract() {
        let mut inbox = DeepLinkInbox::default();
        let (item, _) = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("named"))))
            .unwrap();

        let value = serde_json::to_value(item).unwrap();

        assert_eq!(value["id"], "1");
        assert_eq!(value["type"], "import");
        assert_eq!(value["payload"]["name"], "named");
    }

    #[test]
    fn rejects_new_items_without_evicting_unacknowledged_items_when_full() {
        let mut inbox = DeepLinkInbox::default();
        for index in 0..MAX_PENDING_ITEMS {
            inbox
                .enqueue(DeepLinkInboxPayload::Import(Box::new(request(&format!(
                    "item-{index}"
                )))))
                .unwrap();
        }

        let error = inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(request("overflow"))))
            .expect_err("full inbox must reject new work");

        assert!(error.to_string().contains("inbox is full"));
        assert_eq!(inbox.pending.len(), MAX_PENDING_ITEMS);
        match &inbox.pending.front().unwrap().item.payload {
            DeepLinkInboxPayload::Import(request) => {
                assert_eq!(request.name.as_deref(), Some("item-0"));
            }
            DeepLinkInboxPayload::Error(_) => panic!("oldest import was unexpectedly replaced"),
        }
    }

    #[test]
    fn total_payload_budget_rejects_before_item_limit_and_ack_releases_exact_bytes() {
        let mut inbox = DeepLinkInbox::default();
        let mut large_request = request("large");
        large_request.notes = Some("x".repeat(1024 * 1024));
        let mut accepted = Vec::new();

        loop {
            match inbox.enqueue(DeepLinkInboxPayload::Import(Box::new(
                large_request.clone(),
            ))) {
                Ok((item, _)) => accepted.push(item.id),
                Err(error) => {
                    assert!(error.to_string().contains("payload budget exceeded"));
                    break;
                }
            }
        }

        assert!(accepted.len() < MAX_PENDING_ITEMS);
        let bytes_before_ack = inbox.pending_payload_bytes;
        let first_payload_bytes = inbox.pending.front().unwrap().payload_bytes;
        assert!(inbox.ack(&accepted[0]));
        assert_eq!(
            inbox.pending_payload_bytes,
            bytes_before_ack - first_payload_bytes
        );
        inbox
            .enqueue(DeepLinkInboxPayload::Import(Box::new(large_request)))
            .expect("released payload budget must accept an equivalent item");
        assert_eq!(inbox.pending_payload_bytes, bytes_before_ack);
    }
}
