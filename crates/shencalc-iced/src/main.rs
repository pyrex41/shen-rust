//! ShenCalc — a native, cross-platform symbolic calculator GUI.
//!
//! Pure Rust: the [iced] UI talks straight to the embedded shen-cas engine
//! (`shenffi::CasEngine`) — no FFI, no Swift, no MLX. This is the Syntax-mode
//! MVP (the user types shen-cas bracket syntax and the CAS reduces it). English
//! mode (a small local model via `candle` that maps NL → the CAS tool grammar)
//! is the planned next layer.
//!
//! The CAS reducer is deeply recursive and tree-walked, so — exactly like
//! `ShenCAS.swift` — both boot and every reduce run on a dedicated worker thread
//! with a large stack (the default 8 MB overflows on boot). The UI thread talks
//! to that worker over channels and stays responsive.

mod pretty;

use std::sync::mpsc;
use std::thread;

use iced::futures::channel::oneshot;
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use shenffi::CasEngine;

/// 64 MB — 4× the ~16 MB the reducer needs at depth, matching ShenCAS.swift.
const WORKER_STACK: usize = 64 * 1024 * 1024;

fn main() -> iced::Result {
    // Headless smoke test of the CAS integration (no display needed), so CI and
    // `cargo run -- --selftest` can verify the engine without opening a window.
    if std::env::args().any(|a| a == "--selftest") {
        run_selftest();
        return Ok(());
    }

    iced::application(ShenCalc::boot, ShenCalc::update, ShenCalc::view)
        .title("ShenCalc")
        .run()
}

/// One reduce request handed to the worker thread, with a one-shot reply channel.
struct Job {
    input: String,
    reply: oneshot::Sender<String>,
}

#[derive(Debug, Clone)]
enum Message {
    /// The worker finished booting the CAS (Ok) or failed (Err message).
    Ready(Result<(), String>),
    InputChanged(String),
    Submit,
    /// A reduce finished: the original input plus the rendered result.
    Computed(String, String),
}

struct Entry {
    input: String,
    result: String,
}

struct ShenCalc {
    input: String,
    entries: Vec<Entry>,
    ready: bool,
    status: String,
    /// Outstanding reduces, so the composer can show "working…".
    in_flight: usize,
    /// Request channel to the worker; `None` if the worker died at boot.
    req_tx: Option<mpsc::Sender<Job>>,
}

impl ShenCalc {
    fn boot() -> (Self, Task<Message>) {
        // Spawn the big-stack worker: it boots the engine, reports readiness on
        // `ready_tx`, then serves reduce jobs until the request channel closes.
        let (req_tx, req_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        thread::Builder::new()
            .name("shen-cas".into())
            .stack_size(WORKER_STACK)
            .spawn(move || worker(req_rx, ready_tx))
            .expect("spawn shen-cas worker");

        let state = ShenCalc {
            input: String::new(),
            entries: Vec::new(),
            ready: false,
            status: "starting engine…".into(),
            in_flight: 0,
            req_tx: Some(req_tx),
        };

        // Bridge the readiness one-shot into the iced runtime.
        let ready = Task::perform(
            async move {
                ready_rx
                    .await
                    .unwrap_or_else(|_| Err("worker exited".into()))
            },
            Message::Ready,
        );
        (state, ready)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Ready(Ok(())) => {
                self.ready = true;
                self.status = "engine ready".into();
                Task::none()
            }
            Message::Ready(Err(e)) => {
                self.status = format!("engine failed: {e}");
                self.req_tx = None;
                Task::none()
            }
            Message::InputChanged(s) => {
                self.input = s;
                Task::none()
            }
            Message::Submit => self.submit(),
            Message::Computed(input, result) => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.entries.push(Entry { input, result });
                Task::none()
            }
        }
    }

    fn submit(&mut self) -> Task<Message> {
        let input = self.input.trim().to_string();
        let Some(tx) = self.req_tx.clone() else {
            return Task::none();
        };
        if input.is_empty() || !self.ready {
            return Task::none();
        }
        self.input.clear();
        self.in_flight += 1;

        Task::perform(
            async move {
                let (reply_tx, reply_rx) = oneshot::channel();
                let result = if tx
                    .send(Job {
                        input: input.clone(),
                        reply: reply_tx,
                    })
                    .is_ok()
                {
                    reply_rx
                        .await
                        .unwrap_or_else(|_| "error: worker gone".into())
                } else {
                    "error: engine unavailable".into()
                };
                (input, result)
            },
            |(input, result)| Message::Computed(input, result),
        )
    }

    fn view(&self) -> Element<'_, Message> {
        let transcript = if self.entries.is_empty() {
            column![text(
                "A symbolic calculator powered by the shen-cas engine, in pure \
                 Rust. Type shen-cas syntax, e.g.  D[Sin[x], x]"
            )
            .size(15)]
        } else {
            self.entries.iter().fold(column![].spacing(14), |col, e| {
                let is_error = e.result.starts_with("error:");
                col.push(
                    column![
                        text(e.input.as_str()).size(16).font(iced::Font::MONOSPACE),
                        row![
                            text("= ").size(16),
                            text(e.result.as_str())
                                .size(16)
                                .font(iced::Font::MONOSPACE)
                                .color(if is_error {
                                    iced::Color::from_rgb(0.9, 0.35, 0.35)
                                } else {
                                    iced::Color::from_rgb(0.45, 0.85, 0.72)
                                }),
                        ],
                    ]
                    .spacing(4),
                )
            })
        };

        let working = self.in_flight > 0;
        let composer = row![
            text_input("e.g. D[Sin[x], x]", &self.input)
                .on_input(Message::InputChanged)
                .on_submit(Message::Submit)
                .font(iced::Font::MONOSPACE)
                .padding(10),
            button(text(if working { "…" } else { "=" }))
                .on_press(Message::Submit)
                .padding(10),
        ]
        .spacing(8);

        let header = row![
            text("shen·calc").size(22),
            iced::widget::Space::new().width(Length::Fill),
            text(self.status.as_str()).size(13).color(if self.ready {
                iced::Color::from_rgb(0.45, 0.85, 0.72)
            } else {
                iced::Color::from_rgb(0.85, 0.6, 0.3)
            }),
        ]
        .spacing(8);

        container(
            column![
                header,
                scrollable(transcript)
                    .height(Length::Fill)
                    .width(Length::Fill),
                composer,
            ]
            .spacing(14),
        )
        .padding(18)
        .into()
    }
}

/// Worker-thread entry: boot the CAS, signal readiness, then serve reduces.
fn worker(req_rx: mpsc::Receiver<Job>, ready_tx: oneshot::Sender<Result<(), String>>) {
    let mut engine = match CasEngine::boot() {
        Ok(e) => {
            let _ = ready_tx.send(Ok(()));
            e
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    // `recv` blocks until a job arrives; the loop ends when every Sender drops.
    while let Ok(job) = req_rx.recv() {
        // Reduce to the raw normal form, then humanise it (cos(x), 3·x²) — the
        // same split the Swift apps use (CAS reduce + MathPretty at display).
        let result = pretty::render(&engine.reduce(&job.input));
        let _ = job.reply.send(result);
    }
}

/// Headless verification: boot on a big-stack thread and reduce a fixed battery.
fn run_selftest() {
    let handle = thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(|| {
            let mut engine = match CasEngine::boot() {
                Ok(e) => e,
                Err(e) => {
                    println!("BOOT FAILED: {e}");
                    return false;
                }
            };
            println!("=== ICED SELFTEST START ===");
            let cases = [
                "D[Sin[x], x]",
                "Integrate[x^2, x]",
                "Factor[x^2 - 1]",
                "Solve[x^2 - 4, x]",
                "Expand[(x+1)^2]",
                "6/4",
                "2^10",
            ];
            for c in cases {
                let raw = engine.reduce(c);
                println!("CASE {c}  =>  raw={raw}  pretty={}", pretty::render(&raw));
            }
            println!("=== ICED SELFTEST DONE ===");
            true
        })
        .expect("spawn selftest thread");
    let ok = handle.join().unwrap_or(false);
    if !ok {
        std::process::exit(1);
    }
}
