use crate::PlatformDispatcher;
use async_task::Runnable;
use backtrace::Backtrace;
use collections::{HashMap, VecDeque};
use parking_lot::Mutex;
use rand::prelude::*;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use util::post_inc;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct TestDispatcherId(usize);

pub struct TestDispatcher {
    id: TestDispatcherId,
    state: Arc<Mutex<TestDispatcherState>>,
}

struct TestDispatcherState {
    random: StdRng,
    foreground: HashMap<TestDispatcherId, VecDeque<Runnable>>,
    background: Vec<Runnable>,
    delayed: Vec<(Duration, Runnable)>,
    time: Duration,
    is_main_thread: bool,
    next_id: TestDispatcherId,
    allow_parking: bool,
    waiting_backtrace: Option<Backtrace>,
}

impl TestDispatcher {
    pub fn new(random: StdRng) -> Self {
        let state = TestDispatcherState {
            random,
            foreground: HashMap::default(),
            background: Vec::new(),
            delayed: Vec::new(),
            time: Duration::ZERO,
            is_main_thread: true,
            next_id: TestDispatcherId(1),
            allow_parking: false,
            waiting_backtrace: None,
        };

        TestDispatcher {
            id: TestDispatcherId(0),
            state: Arc::new(Mutex::new(state)),
        }
    }

    pub fn advance_clock(&self, by: Duration) {
        let new_now = self.state.lock().time + by;
        loop {
            self.run_until_parked();
            let state = self.state.lock();
            let next_due_time = state.delayed.first().map(|(time, _)| *time);
            drop(state);
            if let Some(due_time) = next_due_time {
                if due_time <= new_now {
                    self.state.lock().time = due_time;
                    continue;
                }
            }
            break;
        }
        self.state.lock().time = new_now;
    }

    pub fn simulate_random_delay(&self) -> impl 'static + Send + Future<Output = ()> {
        pub struct YieldNow {
            count: usize,
        }

        impl Future for YieldNow {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                if self.count > 0 {
                    self.count -= 1;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }
        }

        YieldNow {
            count: self.state.lock().random.gen_range(0..10),
        }
    }

    pub fn run_until_parked(&self) {
        while self.poll() {}
    }

    pub fn parking_allowed(&self) -> bool {
        self.state.lock().allow_parking
    }

    pub fn allow_parking(&self) {
        self.state.lock().allow_parking = true
    }

    pub fn start_waiting(&self) {
        self.state.lock().waiting_backtrace = Some(Backtrace::new_unresolved());
    }

    pub fn finish_waiting(&self) {
        self.state.lock().waiting_backtrace.take();
    }

    pub fn waiting_backtrace(&self) -> Option<Backtrace> {
        self.state.lock().waiting_backtrace.take().map(|mut b| {
            b.resolve();
            b
        })
    }
}

impl Clone for TestDispatcher {
    fn clone(&self) -> Self {
        let id = post_inc(&mut self.state.lock().next_id.0);
        Self {
            id: TestDispatcherId(id),
            state: self.state.clone(),
        }
    }
}

impl PlatformDispatcher for TestDispatcher {
    fn is_main_thread(&self) -> bool {
        self.state.lock().is_main_thread
    }

    fn dispatch(&self, runnable: Runnable) {
        self.state.lock().background.push(runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: Runnable) {
        self.state
            .lock()
            .foreground
            .entry(self.id)
            .or_default()
            .push_back(runnable);
    }

    fn dispatch_after(&self, duration: std::time::Duration, runnable: Runnable) {
        let mut state = self.state.lock();
        let next_time = state.time + duration;
        let ix = match state.delayed.binary_search_by_key(&next_time, |e| e.0) {
            Ok(ix) | Err(ix) => ix,
        };
        state.delayed.insert(ix, (next_time, runnable));
    }

    fn poll(&self) -> bool {
        let mut state = self.state.lock();

        while let Some((deadline, _)) = state.delayed.first() {
            if *deadline > state.time {
                break;
            }
            let (_, runnable) = state.delayed.remove(0);
            state.background.push(runnable);
        }

        let foreground_len: usize = state
            .foreground
            .values()
            .map(|runnables| runnables.len())
            .sum();
        let background_len = state.background.len();

        if foreground_len == 0 && background_len == 0 {
            return false;
        }

        let main_thread = state.random.gen_ratio(
            foreground_len as u32,
            (foreground_len + background_len) as u32,
        );
        let was_main_thread = state.is_main_thread;
        state.is_main_thread = main_thread;

        let runnable = if main_thread {
            let state = &mut *state;
            let runnables = state
                .foreground
                .values_mut()
                .filter(|runnables| !runnables.is_empty())
                .choose(&mut state.random)
                .unwrap();
            runnables.pop_front().unwrap()
        } else {
            let ix = state.random.gen_range(0..background_len);
            state.background.swap_remove(ix)
        };

        drop(state);
        runnable.run();

        self.state.lock().is_main_thread = was_main_thread;

        true
    }

    fn as_test(&self) -> Option<&TestDispatcher> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Executor;
    use std::sync::Arc;

    #[test]
    fn test_dispatch() {
        let dispatcher = TestDispatcher::new(StdRng::seed_from_u64(0));
        let executor = Executor::new(Arc::new(dispatcher));

        let result = executor.block(async { executor.run_on_main(|| 1).await });
        assert_eq!(result, 1);

        let result = executor.block({
            let executor = executor.clone();
            async move {
                executor
                    .spawn_on_main({
                        let executor = executor.clone();
                        assert!(executor.is_main_thread());
                        || async move {
                            assert!(executor.is_main_thread());
                            let result = executor
                                .spawn({
                                    let executor = executor.clone();
                                    async move {
                                        assert!(!executor.is_main_thread());

                                        let result = executor
                                            .spawn_on_main({
                                                let executor = executor.clone();
                                                || async move {
                                                    assert!(executor.is_main_thread());
                                                    2
                                                }
                                            })
                                            .await;

                                        assert!(!executor.is_main_thread());
                                        result
                                    }
                                })
                                .await;
                            assert!(executor.is_main_thread());
                            result
                        }
                    })
                    .await
            }
        });
        assert_eq!(result, 2);
    }
}