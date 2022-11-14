use std::{
    fs::{self},
    panic::AssertUnwindSafe,
    path::Path,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use bundlers::get_bundlers;
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, BenchmarkId, Criterion,
};
use once_cell::sync::Lazy;
use tokio::{
    runtime::Runtime,
    time::{sleep, timeout},
};
use util::{
    build_test, create_browser, AsyncBencherExtension, PageGuard, PreparedApp, BINDING_NAME,
};

use self::{bundlers::RenderType, util::resume_on_error};
use crate::bundlers::Bundler;

mod bundlers;
mod util;

const MAX_UPDATE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_WARMUP_UPDATE_TIMEOUT: Duration = Duration::from_secs(10);

fn bench_startup(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_startup");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));

    bench_startup_internal(g, false);
}

fn bench_hydration(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_hydration");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));

    bench_startup_internal(g, true);
}

fn bench_startup_internal(mut g: BenchmarkGroup<WallTime>, hydration: bool) {
    let runtime = Runtime::new().unwrap();
    let browser = Lazy::new(|| runtime.block_on(create_browser()));

    for bundler in get_bundlers() {
        let wait_for_hydration = match bundler.render_type() {
            RenderType::ClientSideRendered => {
                // For bundlers without server rendered html "startup" means time to hydration
                // as they only render an empty screen without hydration. Since startup and
                // hydration would be the same we skip the hydration benchmark for them.
                if hydration {
                    continue;
                } else {
                    true
                }
            }
            RenderType::ServerSidePrerendered => hydration,
            RenderType::ServerSideRenderedWithEvents => hydration,
            RenderType::ServerSideRenderedWithoutInteractivity => {
                // For bundlers without interactivity there is no hydration event to wait for
                if hydration {
                    continue;
                } else {
                    false
                }
            }
        };
        for module_count in get_module_counts() {
            let test_app = Lazy::new(|| build_test(module_count, bundler.as_ref()));
            let input = (bundler.as_ref(), &test_app);
            resume_on_error(AssertUnwindSafe(|| {
                g.bench_with_input(
                    BenchmarkId::new(bundler.get_name(), format!("{} modules", module_count)),
                    &input,
                    |b, &(bundler, test_app)| {
                        let browser = &*browser;
                        b.to_async(&runtime).try_iter_async(
                            || async {
                                PreparedApp::new(bundler, test_app.path().to_path_buf()).await
                            },
                            |app| async { Ok(app) },
                            |mut app| async {
                                app.start_server()?;
                                let mut guard = app.with_page(browser).await?;
                                if wait_for_hydration {
                                    guard.wait_for_hydration().await?;
                                }

                                // Defer the dropping of the guard to `teardown`.
                                Ok(guard)
                            },
                            |_guard| async move {},
                        );
                    },
                );
            }));
        }
    }
    g.finish();
}

#[derive(Copy, Clone)]
enum CodeLocation {
    Effect,
    Evaluation,
}

fn bench_hmr_to_eval(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_hmr_to_eval");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));

    bench_hmr_internal(g, CodeLocation::Evaluation);
}

fn bench_hmr_to_commit(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_hmr_to_commit");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));

    bench_hmr_internal(g, CodeLocation::Effect);
}

fn bench_hmr_internal(mut g: BenchmarkGroup<WallTime>, location: CodeLocation) {
    let runtime = Runtime::new().unwrap();
    let browser = Lazy::new(|| runtime.block_on(create_browser()));

    for bundler in get_bundlers() {
        if matches!(
            bundler.render_type(),
            RenderType::ServerSideRenderedWithEvents
                | RenderType::ServerSideRenderedWithoutInteractivity
        ) && matches!(location, CodeLocation::Evaluation)
        {
            // We can't measure evaluation time for these bundlers since it's not evaluated
            // in the browser
            continue;
        }
        for module_count in get_module_counts() {
            let test_app = Lazy::new(|| build_test(module_count, bundler.as_ref()));
            let input = (bundler.as_ref(), &test_app);
            resume_on_error(AssertUnwindSafe(|| {
                g.bench_with_input(
                    BenchmarkId::new(bundler.get_name(), format!("{} modules", module_count)),
                    &input,
                    |b, &(bundler, test_app)| {
                        let browser = &*browser;
                        fn add_code(
                            bundler: &dyn Bundler,
                            app_path: &Path,
                            msg: &str,
                            location: CodeLocation,
                        ) -> Result<()> {
                            let triangle_path = app_path.join("src/triangle.jsx");
                            let mut contents = fs::read_to_string(&triangle_path)?;
                            const INSERTED_CODE_COMMENT: &str = "// Inserted Code:\n";
                            const COMPONENT_START: &str = "function Container({ style }) {\n";
                            const DETECTOR_START: &str = "<Detector ";
                            const DETECTOR_END: &str = "/>";
                            match (location, bundler.render_type()) {
                                (CodeLocation::Effect, _) => {
                                    let a = contents
                                        .find(DETECTOR_START)
                                        .ok_or_else(|| anyhow!("unable to find detector start"))?;
                                    let b = a + contents[a..]
                                        .find(DETECTOR_END)
                                        .ok_or_else(|| anyhow!("unable to find detector end"))?;
                                    contents.replace_range(
                                        a..b,
                                        &format!("{DETECTOR_START}message=\"{msg}\" "),
                                    );
                                }
                                (
                                    CodeLocation::Evaluation,
                                    RenderType::ClientSideRendered
                                    | RenderType::ServerSidePrerendered,
                                ) => {
                                    let b = contents
                                        .find(COMPONENT_START)
                                        .ok_or_else(|| anyhow!("unable to find component start"))?;
                                    let code = format!(
                                        "globalThis.{BINDING_NAME} && \
                                         globalThis.{BINDING_NAME}('{msg}');"
                                    );
                                    if let Some(a) = contents.find(INSERTED_CODE_COMMENT) {
                                        contents.replace_range(
                                            a..b,
                                            &format!("{INSERTED_CODE_COMMENT}{code}\n"),
                                        );
                                    } else {
                                        contents.insert_str(
                                            b,
                                            &format!("{INSERTED_CODE_COMMENT}{code}\n"),
                                        );
                                    }
                                }
                                (
                                    CodeLocation::Evaluation,
                                    RenderType::ServerSideRenderedWithEvents
                                    | RenderType::ServerSideRenderedWithoutInteractivity,
                                ) => {
                                    panic!(
                                        "evaluation can't be measured for bundlers which evaluate \
                                         on server side"
                                    );
                                }
                            }

                            fs::write(&triangle_path, contents)?;
                            Ok(())
                        }
                        static CHANGE_TIMEOUT_MESSAGE: &str =
                            "update was not registered by bundler";
                        async fn make_change<'a>(
                            bundler: &dyn Bundler,
                            guard: &mut PageGuard<'a>,
                            location: CodeLocation,
                            timeout_duration: Duration,
                        ) -> Result<()> {
                            let msg =
                                format!("TURBOPACK_BENCH_CHANGE_{}", guard.app_mut().counter());
                            add_code(bundler, guard.app().path(), &msg, location)?;

                            // Wait for the change introduced above to be reflected at runtime.
                            // This expects HMR or automatic reloading to occur.
                            timeout(timeout_duration, guard.wait_for_binding(&msg))
                                .await
                                .context(CHANGE_TIMEOUT_MESSAGE)??;

                            Ok(())
                        }
                        b.to_async(Runtime::new().unwrap()).try_iter_async(
                            || async {
                                let mut app =
                                    PreparedApp::new(bundler, test_app.path().to_path_buf())
                                        .await?;
                                app.start_server()?;
                                let mut guard = app.with_page(browser).await?;
                                if bundler.has_hydration_event() {
                                    guard.wait_for_hydration().await?;
                                } else {
                                    guard.page().wait_for_navigation().await?;
                                }
                                guard
                                    .page()
                                    .evaluate_expression("globalThis.HMR_IS_HAPPENING = true")
                                    .await
                                    .context(
                                        "Unable to evaluate JavaScript in the page for HMR check \
                                         flag",
                                    )?;
                                Ok(guard)
                            },
                            |mut guard| async move {
                                // Make 5 changes to warm up.
                                for _ in 0..4 {
                                    let _ = make_change(
                                        bundler,
                                        &mut guard,
                                        location,
                                        MAX_WARMUP_UPDATE_TIMEOUT,
                                    )
                                    .await;
                                }
                                let _ =
                                    make_change(bundler, &mut guard, location, MAX_UPDATE_TIMEOUT)
                                        .await;
                                Ok(guard)
                            },
                            |mut guard| async move {
                                make_change(bundler, &mut guard, location, MAX_UPDATE_TIMEOUT)
                                    .await?;

                                // Defer the dropping of the guard to `teardown`.
                                Ok(guard)
                            },
                            |guard| async move {
                                let hmr_is_happening = guard
                                    .page()
                                    .evaluate_expression("globalThis.HMR_IS_HAPPENING")
                                    .await
                                    .unwrap();
                                // Make sure that we are really measuring HMR and not accidentically
                                // full refreshing the page
                                assert!(hmr_is_happening.value().unwrap().as_bool().unwrap());
                            },
                        );
                    },
                );
            }));
        }
    }
}

fn bench_startup_cached(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_startup_cached");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));

    bench_startup_cached_internal(g, false);
}

fn bench_hydration_cached(c: &mut Criterion) {
    let mut g = c.benchmark_group("bench_hydration_cached");
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(60));

    bench_startup_cached_internal(g, true);
}

fn bench_startup_cached_internal(mut g: BenchmarkGroup<WallTime>, hydration: bool) {
    let config = std::env::var("TURBOPACK_BENCH_CACHED").ok();
    if matches!(
        config.as_deref(),
        None | Some("") | Some("no") | Some("false")
    ) {
        return;
    }

    let runtime = Runtime::new().unwrap();
    let browser = Lazy::new(|| runtime.block_on(create_browser()));

    for bundler in get_bundlers() {
        let wait_for_hydration = match bundler.render_type() {
            RenderType::ClientSideRendered => {
                // For bundlers without server rendered html "startup" means time to hydration
                // as they only render an empty screen without hydration. Since startup and
                // hydration would be the same we skip the hydration benchmark for them.
                if hydration {
                    continue;
                } else {
                    true
                }
            }
            RenderType::ServerSidePrerendered => hydration,
            RenderType::ServerSideRenderedWithEvents => hydration,
            RenderType::ServerSideRenderedWithoutInteractivity => {
                // For bundlers without interactivity there is no hydration event to wait for
                if hydration {
                    continue;
                } else {
                    false
                }
            }
        };
        for module_count in get_module_counts() {
            let test_app = Lazy::new(|| build_test(module_count, bundler.as_ref()));
            let input = (bundler.as_ref(), &test_app);

            resume_on_error(AssertUnwindSafe(|| {
                g.bench_with_input(
                    BenchmarkId::new(bundler.get_name(), format!("{} modules", module_count)),
                    &input,
                    |b, &(bundler, test_app)| {
                        let browser = &*browser;
                        b.to_async(Runtime::new().unwrap()).try_iter_async(
                            || async {
                                // Run a complete build, shut down, and test running it again
                                let mut app =
                                    PreparedApp::new(bundler, test_app.path().to_path_buf())
                                        .await?;
                                app.start_server()?;
                                let mut guard = app.with_page(browser).await?;
                                if bundler.has_hydration_event() {
                                    guard.wait_for_hydration().await?;
                                } else {
                                    guard.page().wait_for_navigation().await?;
                                }

                                let mut app = guard.close_page().await?;

                                // Give it 4 seconds time to store the cache
                                sleep(Duration::from_secs(4)).await;

                                app.stop_server()?;
                                Ok(app)
                            },
                            |app| async { Ok(app) },
                            |mut app| async {
                                app.start_server()?;
                                let mut guard = app.with_page(browser).await?;
                                if wait_for_hydration {
                                    guard.wait_for_hydration().await?;
                                }

                                // Defer the dropping of the guard to `teardown`.
                                Ok(guard)
                            },
                            |_guard| async move {},
                        );
                    },
                );
            }));
        }
    }
}

fn get_module_counts() -> Vec<usize> {
    let config = std::env::var("TURBOPACK_BENCH_COUNTS").ok();
    match config.as_deref() {
        None | Some("") => {
            vec![1_000]
        }
        Some(config) => config
            .split(',')
            .map(|s| s.parse().expect("Invalid value for TURBOPACK_BENCH_COUNTS"))
            .collect(),
    }
}

criterion_group!(
    name = benches;
    config = Criterion::default();
    targets = bench_startup, bench_hydration, bench_startup_cached, bench_hydration_cached, bench_hmr_to_eval, bench_hmr_to_commit
);
criterion_main!(benches);
