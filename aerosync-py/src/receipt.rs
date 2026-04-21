//! `Receipt` — sender-side state-machine handle exposed to Python.
//!
//! Wraps `Arc<aerosync::core::receipt::Receipt<Sender>>`, which is the
//! canonical source of truth for the lifecycle of a single send. The
//! Python surface mirrors RFC-001 §5.5 modulo the `progress()`
//! iterator (which is wired separately by w6 task #13's
//! `on_progress` callback path).
//!
//! # State strings
//!
//! `PyReceipt::state` returns the protocol-canonical lowercase
//! identifier from the `Display` impl on `State` — `initiated`,
//! `stream_opened`, `data_transferred`, `stream_closed`, `processing`,
//! `completed`, `failed`. Terminal payloads (Acked / Nacked /
//! Cancelled / Errored) are flattened to the bare `completed` /
//! `failed` strings; the structured terminal reason lives in the
//! `Outcome` dict returned by `PyReceipt::processed`.
//!
//! # Awaitables
//!
//! `sent`, `received`, `processed`, `cancel` all return Python
//! awaitables backed by `tokio::sync::watch::Receiver::changed()`
//! through [`Receipt::await_state`]. The Python user awaits them
//! exactly like any asyncio coroutine.
//!
//! `watch()` returns an async iterator (a [`PyReceiptWatcher`]
//! `#[pyclass]`) yielding the state string on every change. It uses
//! the same `watch::Receiver` channel; `tokio::sync::watch` may
//! coalesce intermediate values when the consumer lags, so do not
//! depend on observing every intermediate state — the channel
//! guarantees only the *latest* state, plus a final terminal
//! observation before close.

use crate::runtime::future_into_py;
use aerosync::core::receipt::{Event, Outcome, Receipt, Sender, State};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::sync::Arc;
use tokio::sync::{watch, Mutex as AsyncMutex};

/// Canonical Python label for a generic [`State`]. Terminal payloads
/// are flattened to bare `"completed"` / `"failed"` — the rich reason
/// lives in the [`Outcome`] dict.
pub(crate) fn state_label(s: &State) -> &'static str {
    match s {
        State::Initiated => "initiated",
        State::StreamOpened => "stream_opened",
        State::DataTransferred => "data_transferred",
        State::StreamClosed => "stream_closed",
        State::Processing => "processing",
        State::Completed(_) => "completed",
        State::Failed(_) => "failed",
    }
}

/// Build the `Outcome` dict returned by `await receipt.processed()`.
///
/// Shape — stable as part of the v0.2 API:
///
/// | Key       | Type                | Notes                              |
/// | --------- | ------------------- | ---------------------------------- |
/// | `status`  | `str`               | `"acked"` / `"nacked"` / `"cancelled"` / `"errored"` |
/// | `reason`  | `str | None`        | Free-form for nack/cancel; `None` for acked |
/// | `code`    | `int | None`        | Numeric error code for errored; `None` otherwise |
/// | `detail`  | `str | None`        | Human-readable detail for errored; `None` otherwise |
pub(crate) fn outcome_to_pydict<'py>(
    py: Python<'py>,
    outcome: &Outcome,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    match outcome {
        Outcome::Acked => {
            d.set_item("status", "acked")?;
            d.set_item("reason", py.None())?;
            d.set_item("code", py.None())?;
            d.set_item("detail", py.None())?;
        }
        Outcome::Nacked(reason) => {
            d.set_item("status", "nacked")?;
            d.set_item("reason", reason)?;
            d.set_item("code", py.None())?;
            d.set_item("detail", py.None())?;
        }
        Outcome::Cancelled(reason) => {
            d.set_item("status", "cancelled")?;
            d.set_item("reason", reason)?;
            d.set_item("code", py.None())?;
            d.set_item("detail", py.None())?;
        }
        Outcome::Failed { code, detail } => {
            d.set_item("status", "errored")?;
            d.set_item("reason", py.None())?;
            d.set_item("code", *code)?;
            d.set_item("detail", detail)?;
        }
    }
    Ok(d)
}

/// Sender-side `Receipt` handle.
#[pyclass(module = "aerosync._native", name = "Receipt")]
pub struct PyReceipt {
    pub(crate) inner: Arc<Receipt<Sender>>,
}

impl PyReceipt {
    pub fn from_arc(inner: Arc<Receipt<Sender>>) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyReceipt {
    /// UUID v7 string identifier, stable across state transitions.
    #[getter]
    fn id(&self) -> String {
        self.inner.id().to_string()
    }

    /// Current canonical state label. Cheap snapshot; safe to call
    /// from any thread.
    #[getter]
    fn state(&self) -> &'static str {
        state_label(&self.inner.state())
    }

    /// `await receipt.sent()` — resolves once the underlying receipt
    /// has progressed at least to `StreamClosed` (all bytes flushed
    /// and the QUIC FIN observed). Terminal states also satisfy this
    /// predicate; callers should inspect `state` afterwards.
    fn sent<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py(py, async move {
            let s = inner
                .await_state(|s| {
                    matches!(
                        s,
                        State::StreamClosed
                            | State::Processing
                            | State::Completed(_)
                            | State::Failed(_)
                    )
                })
                .await;
            Ok(state_label(&s))
        })
    }

    /// `await receipt.received()` — resolves once the receiver-side
    /// application has picked up the payload (state `Processing`)
    /// or the receipt has reached a terminal state.
    fn received<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py(py, async move {
            let s = inner
                .await_state(|s| {
                    matches!(
                        s,
                        State::Processing | State::Completed(_) | State::Failed(_)
                    )
                })
                .await;
            Ok(state_label(&s))
        })
    }

    /// `await receipt.processed()` — resolves once a terminal state
    /// is observed. Returns the structured `Outcome` dict (see
    /// module docs). Never raises on terminal failure; callers
    /// inspect `outcome["status"]` to discriminate ack vs nack vs
    /// cancel vs error.
    fn processed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py(py, async move {
            let outcome = inner.processed().await;
            Python::attach(|py| {
                let d = outcome_to_pydict(py, &outcome)?;
                Ok(d.unbind())
            })
        })
    }

    /// `await receipt.cancel(reason)` — applies `Event::Cancel` to
    /// the local sender-side receipt (RFC-002 §4). The state
    /// transitions to `Failed(Cancelled)` if the receipt is
    /// non-terminal; raises `ValueError` if the transition is
    /// rejected (e.g. receipt is already terminal).
    ///
    /// Note: this only mutates the *local* sender-side state
    /// machine. Propagation to the remote receiver requires the QUIC
    /// receipt-stream wiring (RFC-002 §6.2), which is engine-side
    /// work tracked outside this binding.
    #[pyo3(signature = (reason = "user-cancelled".to_string()))]
    fn cancel<'py>(&self, py: Python<'py>, reason: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py(py, async move {
            inner
                .apply_event(Event::Cancel { reason })
                .map_err(|e| PyValueError::new_err(format!("cancel rejected: {e}")))?;
            Ok(state_label(&inner.state()))
        })
    }

    /// `async for state in receipt.watch(): ...` — async iterator
    /// over state transitions. Yields the canonical state label on
    /// every change observed by the underlying `watch::Receiver`.
    /// The iterator terminates with `StopAsyncIteration` once the
    /// receipt reaches a terminal state.
    fn watch(&self) -> PyReceiptWatcher {
        PyReceiptWatcher {
            rx: Arc::new(AsyncMutex::new(self.inner.watch())),
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Receipt(id={:?}, state={:?})",
            self.inner.id().to_string(),
            state_label(&self.inner.state())
        )
    }
}

/// Async iterator over a [`PyReceipt`]'s state-change stream.
///
/// Holds a `watch::Receiver<State>` clone; multiple watchers on the
/// same receipt are independent. Tokio's watch channel coalesces
/// intermediate values if the consumer lags, so this iterator yields
/// the *latest* state at each tick rather than every transition. A
/// terminal state is always observed (and yielded) at least once
/// before `StopAsyncIteration`.
#[pyclass(module = "aerosync._native", name = "ReceiptWatcher")]
pub struct PyReceiptWatcher {
    rx: Arc<AsyncMutex<watch::Receiver<State>>>,
    done: Arc<std::sync::atomic::AtomicBool>,
}

#[pymethods]
impl PyReceiptWatcher {
    fn __aiter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __anext__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = Arc::clone(&slf.bind(py).borrow().rx);
        let done = Arc::clone(&slf.bind(py).borrow().done);
        future_into_py(py, async move {
            use pyo3::exceptions::PyStopAsyncIteration;
            use std::sync::atomic::Ordering;
            if done.load(Ordering::SeqCst) {
                return Err(PyStopAsyncIteration::new_err(()));
            }
            let mut guard = rx.lock().await;
            // First call (or any call after a previously observed
            // mutation): if `borrow_and_update` shows a value we
            // haven't yielded yet we hand it back. Otherwise wait
            // for the next change.
            let state = {
                let s = guard.borrow_and_update().clone();
                if s != State::Initiated {
                    Some(s)
                } else {
                    None
                }
            };
            let state = match state {
                Some(s) => s,
                None => match guard.changed().await {
                    Ok(()) => guard.borrow_and_update().clone(),
                    Err(_) => {
                        done.store(true, Ordering::SeqCst);
                        return Err(PyStopAsyncIteration::new_err(()));
                    }
                },
            };
            if state.is_terminal() {
                done.store(true, Ordering::SeqCst);
            }
            Ok(state_label(&state))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aerosync::core::receipt::{CompletedTerminal, FailedTerminal};
    use uuid::Uuid;

    #[test]
    fn state_label_covers_every_variant() {
        assert_eq!(state_label(&State::Initiated), "initiated");
        assert_eq!(state_label(&State::StreamOpened), "stream_opened");
        assert_eq!(state_label(&State::DataTransferred), "data_transferred");
        assert_eq!(state_label(&State::StreamClosed), "stream_closed");
        assert_eq!(state_label(&State::Processing), "processing");
        assert_eq!(
            state_label(&State::Completed(CompletedTerminal::Acked)),
            "completed"
        );
        assert_eq!(
            state_label(&State::Failed(FailedTerminal::Cancelled {
                reason: "x".into()
            })),
            "failed"
        );
    }

    #[test]
    fn from_arc_round_trips_id() {
        let id = Uuid::new_v4();
        let r: Arc<Receipt<Sender>> = Arc::new(Receipt::new(id));
        let py = PyReceipt::from_arc(Arc::clone(&r));
        assert_eq!(py.id(), id.to_string());
        assert_eq!(py.state(), "initiated");
    }

    #[test]
    fn cancel_transitions_to_failed() {
        let r: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        // Direct apply_event keeps this test free of pyo3 / runtime.
        r.apply_event(Event::Cancel {
            reason: "user".into(),
        })
        .unwrap();
        let py = PyReceipt::from_arc(Arc::clone(&r));
        assert_eq!(py.state(), "failed");
    }

    #[tokio::test]
    async fn processed_resolves_after_ack() {
        let r: Arc<Receipt<Sender>> = Arc::new(Receipt::new(Uuid::new_v4()));
        r.apply_event(Event::Open).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Close).unwrap();
        r.apply_event(Event::Process).unwrap();
        r.apply_event(Event::Ack).unwrap();
        let outcome = r.processed().await;
        assert_eq!(outcome, Outcome::Acked);
    }
}
