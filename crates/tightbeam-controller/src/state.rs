use crate::conversation::ConversationLog;
use crate::crd::TightbeamModelSpec;
use std::sync::Arc;
use tightbeam_proto::{InboundMessage, TurnAssignment, TurnResultChunk};
use tokio::sync::{broadcast, mpsc, Mutex, Notify, RwLock};

pub struct PendingTurn {
    pub assignment: TurnAssignment,
    pub result_tx: mpsc::Sender<TurnResultChunk>,
}

pub enum JobAction {
    AlreadyConnected,
    NoKubeClient,
    NoModelSpec,
    Create(TightbeamModelSpec),
}

pub struct ControllerState {
    pub conversation: Arc<RwLock<ConversationLog>>,
    pending_tx: mpsc::Sender<PendingTurn>,
    pending_rx: Mutex<mpsc::Receiver<PendingTurn>>,
    active_result_tx: Mutex<Option<mpsc::Sender<TurnResultChunk>>>,
    subscriber_tx: broadcast::Sender<InboundMessage>,
    job_connected: Mutex<bool>,
    job_notify: Notify,
    kube_client: Option<kube::Client>,
    namespace: String,
    controller_addr: String,
    model_spec: Mutex<Option<TightbeamModelSpec>>,
}

impl ControllerState {
    pub fn new(
        conversation: ConversationLog,
        kube_client: Option<kube::Client>,
        namespace: String,
        controller_addr: String,
    ) -> Self {
        let (pending_tx, pending_rx) = mpsc::channel(1);
        let (subscriber_tx, _) = broadcast::channel(16);
        Self {
            conversation: Arc::new(RwLock::new(conversation)),
            pending_tx,
            pending_rx: Mutex::new(pending_rx),
            active_result_tx: Mutex::new(None),
            subscriber_tx,
            job_connected: Mutex::new(false),
            job_notify: Notify::new(),
            kube_client,
            namespace,
            controller_addr,
            model_spec: Mutex::new(None),
        }
    }

    pub async fn enqueue_turn(&self, pending: PendingTurn) -> Result<(), String> {
        tracing::info!("enqueue_turn: sending pending turn");
        let result = self
            .pending_tx
            .send(pending)
            .await
            .map_err(|_| "turn queue closed".to_string());
        tracing::info!("enqueue_turn: complete, ok={}", result.is_ok());
        result
    }

    pub async fn wait_for_turn(&self) -> Option<PendingTurn> {
        tracing::info!("wait_for_turn: acquiring lock");
        let mut rx = self.pending_rx.lock().await;
        tracing::info!("wait_for_turn: lock acquired, waiting for message");
        let result = rx.recv().await;
        tracing::info!("wait_for_turn: recv complete, got={}", result.is_some());
        result
    }

    pub async fn set_active_result_tx(&self, tx: mpsc::Sender<TurnResultChunk>) {
        tracing::info!("set_active_result_tx");
        *self.active_result_tx.lock().await = Some(tx);
    }

    pub async fn take_active_result_tx(&self) -> Option<mpsc::Sender<TurnResultChunk>> {
        let result = self.active_result_tx.lock().await.take();
        tracing::info!("take_active_result_tx: found={}", result.is_some());
        result
    }

    pub fn kube_client(&self) -> Option<&kube::Client> {
        self.kube_client.as_ref()
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn controller_addr(&self) -> &str {
        &self.controller_addr
    }

    pub fn subscribe(&self) -> broadcast::Receiver<InboundMessage> {
        self.subscriber_tx.subscribe()
    }

    pub fn notify_subscriber(&self, message: InboundMessage) {
        let _ = self.subscriber_tx.send(message);
    }

    pub async fn set_model_spec(&self, spec: TightbeamModelSpec) {
        *self.model_spec.lock().await = Some(spec);
    }

    pub async fn clear_model_spec(&self) {
        *self.model_spec.lock().await = None;
    }

    pub async fn check_job_needed(&self) -> JobAction {
        if self.is_job_connected().await {
            return JobAction::AlreadyConnected;
        }
        if self.kube_client.is_none() {
            return JobAction::NoKubeClient;
        }
        let spec = self.model_spec.lock().await;
        match &*spec {
            Some(s) => JobAction::Create(s.clone()),
            None => JobAction::NoModelSpec,
        }
    }

    pub async fn is_job_connected(&self) -> bool {
        *self.job_connected.lock().await
    }

    pub async fn set_job_connected(&self, connected: bool) {
        *self.job_connected.lock().await = connected;
        if connected {
            self.job_notify.notify_waiters();
        }
    }

    pub async fn wait_for_job_connect(&self, timeout: std::time::Duration) -> bool {
        if self.is_job_connected().await {
            return true;
        }
        tokio::time::timeout(timeout, self.job_notify.notified())
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::ConversationLog;

    fn make_state() -> ControllerState {
        let tmp = tempfile::TempDir::new().unwrap();
        let conv = ConversationLog::new(tmp.path());
        std::mem::forget(tmp);
        ControllerState::new(conv, None, "default".into(), "http://localhost:9090".into())
    }

    #[tokio::test]
    async fn enqueue_and_wait_delivers() {
        let state = Arc::new(make_state());
        let (result_tx, _result_rx) = mpsc::channel(1);

        let pending = PendingTurn {
            assignment: TurnAssignment {
                system: Some("test".into()),
                tools: vec![],
                messages: vec![],
                model_config: None,
            },
            result_tx,
        };

        let state_clone = state.clone();
        let handle = tokio::spawn(async move { state_clone.wait_for_turn().await });

        state.enqueue_turn(pending).await.unwrap();
        let received = handle.await.unwrap().unwrap();
        assert_eq!(received.assignment.system, Some("test".into()));
    }

    #[tokio::test]
    async fn take_active_result_tx_returns_none_when_empty() {
        let state = make_state();
        assert!(state.take_active_result_tx().await.is_none());
    }

    #[tokio::test]
    async fn set_then_take_active_result_tx() {
        let state = make_state();
        let (tx, _rx) = mpsc::channel::<TurnResultChunk>(1);

        state.set_active_result_tx(tx).await;
        assert!(state.take_active_result_tx().await.is_some());
        assert!(
            state.take_active_result_tx().await.is_none(),
            "second take should return None"
        );
    }

    #[tokio::test]
    async fn job_connected_starts_false() {
        let state = make_state();
        assert!(!state.is_job_connected().await);
    }

    #[tokio::test]
    async fn set_job_connected_and_check() {
        let state = make_state();
        state.set_job_connected(true).await;
        assert!(state.is_job_connected().await);
        state.set_job_connected(false).await;
        assert!(!state.is_job_connected().await);
    }

    #[tokio::test]
    async fn wait_for_job_connect_returns_true_when_already_connected() {
        let state = make_state();
        state.set_job_connected(true).await;
        assert!(
            state
                .wait_for_job_connect(std::time::Duration::from_millis(10))
                .await
        );
    }

    #[tokio::test]
    async fn wait_for_job_connect_times_out() {
        let state = make_state();
        assert!(
            !state
                .wait_for_job_connect(std::time::Duration::from_millis(10))
                .await
        );
    }

    #[tokio::test]
    async fn check_job_needed_already_connected() {
        let state = make_state();
        state.set_job_connected(true).await;
        assert!(matches!(
            state.check_job_needed().await,
            JobAction::AlreadyConnected
        ));
    }

    #[tokio::test]
    async fn check_job_needed_no_kube_client() {
        let state = make_state(); // kube_client is None
        assert!(matches!(
            state.check_job_needed().await,
            JobAction::NoKubeClient
        ));
    }

    #[tokio::test]
    async fn check_job_needed_no_kube_client_even_with_spec() {
        let state = make_state();
        state
            .set_model_spec(TightbeamModelSpec {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-20250514".into(),
                secret_name: "llm-key".into(),
                max_tokens: 8192,
                image: "ghcr.io/test:latest".into(),
                idle_timeout: 300,
                description: String::new(),
            })
            .await;
        // kube_client is None, so NoKubeClient takes priority over Create
        assert!(matches!(
            state.check_job_needed().await,
            JobAction::NoKubeClient
        ));
    }

    #[tokio::test]
    async fn wait_for_job_connect_wakes_on_notify() {
        let state = Arc::new(make_state());
        let state2 = state.clone();

        let handle = tokio::spawn(async move {
            state2
                .wait_for_job_connect(std::time::Duration::from_secs(5))
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        state.set_job_connected(true).await;

        assert!(handle.await.unwrap());
    }
}
