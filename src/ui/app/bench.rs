use std::time::{Duration, Instant};

use ratatui::{Terminal, backend::TestBackend};

use super::*;

/// Summary stats (microseconds) over a sample of durations.
struct Stats {
    n: usize,
    min_us: u128,
    p50_us: u128,
    p99_us: u128,
    max_us: u128,
    mean_us: u128,
}

fn stats(mut samples: Vec<Duration>) -> Stats {
    samples.sort_unstable();
    let n = samples.len().max(1);
    let us = |d: &Duration| d.as_micros();
    let at = |frac_num: usize, frac_den: usize| {
        let idx = ((n.saturating_sub(1)) * frac_num) / frac_den;
        us(&samples[idx.min(n - 1)])
    };
    let sum: u128 = samples.iter().map(us).sum();
    Stats {
        n: samples.len(),
        min_us: us(&samples[0]),
        p50_us: at(1, 2),
        p99_us: at(99, 100),
        max_us: us(&samples[n - 1]),
        mean_us: sum / n as u128,
    }
}

fn print_stats(label: &str, s: &Stats) {
    println!(
        "  {label:<22} n={:<4} min={:>8.3}ms  p50={:>8.3}ms  p99={:>8.3}ms  max={:>8.3}ms  mean={:>8.3}ms",
        s.n,
        s.min_us as f64 / 1000.0,
        s.p50_us as f64 / 1000.0,
        s.p99_us as f64 / 1000.0,
        s.max_us as f64 / 1000.0,
        s.mean_us as f64 / 1000.0,
    );
}

/// Current process resident set size in KB (macOS/Linux via `ps`).
fn current_rss_kb() -> Option<u64> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

impl App {
    pub(super) async fn run_bench(
        &mut self,
        mut delta_rx: mpsc::Receiver<StateDelta>,
        iters: usize,
        settle_secs: u64,
    ) {
        // Scope to all-namespace Pods — the densest list and the primary scale target.
        let pods_idx = ResourceKind::ORDERED
            .iter()
            .position(|k| *k == ResourceKind::Pods)
            .unwrap_or(0);
        {
            let tab = self.current_tab_mut();
            tab.kind_idx = pods_idx;
            tab.namespace = None;
        }
        let context = self.current_tab().context.clone();

        eprintln!("[bench] context={context} kind=Pods ns=all settle={settle_secs}s iters={iters}");
        eprintln!("[bench] starting watchers ...");
        self.ensure_active_watch().await;

        // ---- settle / initial-sync: drain deltas until entity count stabilizes ----
        let start = Instant::now();
        let mut deltas = 0u64;
        let mut last_count = 0usize;
        let mut stable_since: Option<Instant> = None;
        let mut sync_elapsed: Option<Duration> = None;
        while start.elapsed() < Duration::from_secs(settle_secs) {
            while let Ok(delta) = delta_rx.try_recv() {
                self.store.apply(delta);
                deltas += 1;
            }
            let count = self.store.entity_count();
            if count > 0 && count == last_count {
                let since = *stable_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_millis(1500) {
                    sync_elapsed = Some(start.elapsed());
                    break;
                }
            } else {
                stable_since = None;
            }
            last_count = count;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let drain_elapsed = start.elapsed();
        let sync_elapsed = sync_elapsed.unwrap_or(drain_elapsed);

        let active = self.current_tab().clone();
        let request = self.view_request_for_tab(&active);
        let pod_rows = self.store.count(&context, ResourceKind::Pods, None);
        let total_entities = self.store.entity_count();

        // ---- projection (cache-bypassed: raw recompute cost) ----
        let mut proj = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let vm = self.projector.project(&self.store, &request);
            std::hint::black_box(&vm);
            proj.push(t.elapsed());
        }

        // ---- cluster-wide pulse aggregate (the 2.2 hot path: sum every pod's extracted fields) ----
        let mut pulse = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let pods = self.store.list(&context, ResourceKind::Pods, None);
            let mut acc = 0u64;
            for pod in pods {
                if let Some(res) = &pod.extracted.pod_resources {
                    acc = acc
                        .wrapping_add(res.cpu_request_m)
                        .wrapping_add(res.mem_request_b);
                }
            }
            std::hint::black_box(acc);
            pulse.push(t.elapsed());
        }

        // ---- full frame render via TestBackend (steady-state: projection cache warm) ----
        let backend = TestBackend::new(200, 50);
        let mut terminal = Terminal::new(backend).expect("test backend terminal");
        let _ = terminal.draw(|f| self.draw(f)); // warm
        let mut draw = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            let _ = terminal.draw(|f| self.draw(f));
            draw.push(t.elapsed());
        }

        let rss_kb = current_rss_kb();

        println!("\n=== krust bench report ===");
        println!("  entities (store):      {total_entities}  (pods in ctx, all-ns: {pod_rows})");
        println!(
            "  initial sync:          {:.2}s to stabilize  ({deltas} deltas drained, {:.0} deltas/s)",
            sync_elapsed.as_secs_f64(),
            deltas as f64 / drain_elapsed.as_secs_f64().max(0.001),
        );
        match rss_kb {
            Some(kb) => println!(
                "  RSS:                   {kb} KB  ({:.1} MB)",
                kb as f64 / 1024.0
            ),
            None => println!("  RSS:                   (unavailable)"),
        }
        println!("  --- hot paths ({iters} iters) ---");
        print_stats("projection (recompute)", &stats(proj));
        print_stats("pulse aggregate", &stats(pulse));
        print_stats("frame render (cached)", &stats(draw));
        println!("==========================\n");
    }
}
