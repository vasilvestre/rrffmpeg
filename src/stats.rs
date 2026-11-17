//! Read the journal back and summarize what it says.
//!
//! rrffmpeg is one process per job, so there is no daemon to scrape and no
//! port to open. After the job ends, the journal is the only place its numbers
//! exist. This module turns those lines into the two figures that matter: how
//! long the dispatcher took before the worker started, and how long the job
//! itself ran.

use crate::Error;
use crate::Result;
use crate::state::{Event, records};
use std::collections::BTreeMap;
use std::path::Path;

/// Microseconds at a rank, nearest rank, on a sorted slice.
pub fn percentile(sorted: &[u64], fraction: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((fraction * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[index - 1]
}

#[derive(Debug, Default, Clone)]
pub struct HostTally {
    pub jobs: usize,
    pub failed: usize,
    pub down: usize,
    pub up: usize,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub host: String,
    /// Microseconds from process start to the moment the job went out.
    pub dispatch_us: u64,
    pub started_ts: u64,
    /// Set once a Finished event arrives.
    pub duration_us: Option<u64>,
    pub code: Option<i32>,
    pub reaped: bool,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub path: String,
    pub records: usize,
    /// Sorted when read, so percentiles are a lookup.
    pub dispatch_us: Vec<u64>,
    pub duration_us: Vec<u64>,
    pub hosts: BTreeMap<String, HostTally>,
    pub codes: BTreeMap<i32, usize>,
    pub jobs: Vec<Job>,
    pub open: usize,
    pub reaped: usize,
}

impl Summary {
    pub fn of(dir: &Path) -> Result<Self> {
        let records = records(dir)?;
        Ok(Self::from_records(dir.display().to_string(), &records))
    }

    pub fn from_records(path: String, records: &[crate::state::Record]) -> Self {
        let mut summary = Self {
            path,
            records: records.len(),
            ..Self::default()
        };
        let mut index: BTreeMap<String, usize> = BTreeMap::new();

        for record in records {
            match &record.ev {
                Event::Started {
                    job,
                    host,
                    elapsed_us,
                } => {
                    let slot = summary.jobs.len();
                    index.insert(job.clone(), slot);
                    summary.jobs.push(Job {
                        id: job.clone(),
                        host: host.clone(),
                        dispatch_us: *elapsed_us,
                        started_ts: record.ts,
                        duration_us: None,
                        code: None,
                        reaped: false,
                    });
                    let tally = summary.hosts.entry(host.clone()).or_default();
                    tally.jobs += 1;
                }
                Event::Finished { job, code } => {
                    let Some(&slot) = index.get(job) else {
                        continue;
                    };
                    let started = summary.jobs[slot].started_ts;
                    let job = &mut summary.jobs[slot];
                    job.code = Some(*code);
                    job.duration_us = Some(record.ts.saturating_sub(started));
                    if *code != 0 {
                        let host = job.host.clone();
                        summary.hosts.entry(host).or_default().failed += 1;
                    }
                    *summary.codes.entry(*code).or_insert(0) += 1;
                }
                Event::Reaped { job } => {
                    summary.reaped += 1;
                    if let Some(&slot) = index.get(job) {
                        summary.jobs[slot].reaped = true;
                    }
                }
                Event::HostDown { host } => {
                    summary.hosts.entry(host.clone()).or_default().down += 1;
                }
                Event::HostUp { host } => {
                    summary.hosts.entry(host.clone()).or_default().up += 1;
                }
            }
        }

        summary.open = summary
            .jobs
            .iter()
            .filter(|job| job.code.is_none() && !job.reaped)
            .count();
        summary.dispatch_us = summary.jobs.iter().map(|job| job.dispatch_us).collect();
        summary.dispatch_us.sort_unstable();
        summary.duration_us = summary
            .jobs
            .iter()
            .filter_map(|job| job.duration_us)
            .collect();
        summary.duration_us.sort_unstable();
        summary
    }

    pub fn report(&self) -> String {
        let mut out = format!("journal {}\n", self.path);
        out.push_str(&format!(
            "records {}  jobs {}  open {}  reaped {}\n",
            self.records,
            self.jobs.len(),
            self.open,
            self.reaped
        ));
        out.push_str(&self.line("dispatch", &self.dispatch_us));
        out.push_str(&self.line("duration", &self.duration_us));
        if !self.codes.is_empty() {
            let codes: Vec<String> = self
                .codes
                .iter()
                .map(|(code, n)| format!("{code}:{n}"))
                .collect();
            out.push_str(&format!("exit codes  {}\n", codes.join(" ")));
        }
        for (host, tally) in &self.hosts {
            out.push_str(&format!(
                "host {host}  jobs {}  failed {}  down {}  up {}\n",
                tally.jobs, tally.failed, tally.down, tally.up
            ));
        }
        out
    }

    fn line(&self, label: &str, samples: &[u64]) -> String {
        if samples.is_empty() {
            return format!("{label:<9} none\n");
        }
        format!(
            "{label:<9} n={:<5} p50 {:<9} p95 {:<9} p99 {:<9} max {}\n",
            samples.len(),
            human(percentile(samples, 0.50)),
            human(percentile(samples, 0.95)),
            human(percentile(samples, 0.99)),
            human(percentile(samples, 1.0)),
        )
    }

    pub fn json(&self) -> String {
        let value = serde_json::json!({
            "journal": self.path,
            "records": self.records,
            "jobs": self.jobs.len(),
            "open": self.open,
            "reaped": self.reaped,
            "dispatch_us": stats(&self.dispatch_us),
            "duration_us": stats(&self.duration_us),
        });
        value.to_string()
    }
}

fn stats(samples: &[u64]) -> serde_json::Value {
    serde_json::json!({
        "n": samples.len(),
        "p50": percentile(samples, 0.50),
        "p95": percentile(samples, 0.95),
        "p99": percentile(samples, 0.99),
        "max": percentile(samples, 1.0),
    })
}

/// Microseconds in the unit a reader thinks in. Two digits are enough at every
/// scale: nobody needs 12.4 s shown to the microsecond.
pub fn human(us: u64) -> String {
    match us {
        0..=999 => format!("{us} us"),
        1_000..=999_999 => format!("{:.2} ms", us as f64 / 1_000.0),
        1_000_000..=59_999_999 => format!("{:.1} s", us as f64 / 1_000_000.0),
        _ => format!("{:.2} min", us as f64 / 60_000_000.0),
    }
}

/// Load the config and print the summary, or one JSON object.
pub fn main(args: &[String]) -> Result<i32> {
    let mut dir = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--dir" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err(Error::Config("--dir needs a path".into()));
                };
                dir = Some(std::path::PathBuf::from(value));
            }
            "-h" | "--help" => {
                eprintln!("usage: rrffmpeg-stats [--dir STATE_DIR] [--json]");
                return Ok(0);
            }
            other => {
                return Err(Error::Config(format!("unknown option {other}")));
            }
        }
        i += 1;
    }
    let dir = match dir {
        Some(dir) => dir,
        None => crate::Config::load()?.state_dir,
    };
    let summary = Summary::of(&dir)?;
    if json {
        println!("{}", summary.json());
    } else {
        print!("{}", summary.report());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let samples = [1, 2, 3, 4];
        assert_eq!(percentile(&samples, 0.50), 2);
        assert_eq!(percentile(&samples, 0.95), 4);
        assert_eq!(percentile(&samples, 1.0), 4);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn human_units() {
        assert_eq!(human(412), "412 us");
        assert_eq!(human(1_500), "1.50 ms");
        assert_eq!(human(12_400_000), "12.4 s");
        assert_eq!(human(120_000_000), "2.00 min");
    }

    #[test]
    fn a_started_and_a_finished_make_one_measured_job() {
        let path = "journal".to_string();
        let records = vec![
            record(1, 1_000, started("a", "worker", 400)),
            record(2, 1_300_000, finished("a", 0)),
            record(3, 2_000, started("b", "worker", 900)),
        ];
        let summary = Summary::from_records(path, &records);
        assert_eq!(summary.jobs.len(), 2);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.duration_us, vec![1_299_000]);
        assert_eq!(summary.codes.get(&0), Some(&1));
        assert_eq!(summary.hosts["worker"].jobs, 2);
        let report = summary.report();
        assert!(report.contains("dispatch"), "{report}");
        assert!(report.contains("host worker"), "{report}");
    }

    #[test]
    fn a_failed_job_is_counted_on_its_host() {
        let records = vec![
            record(1, 0, started("a", "worker", 500)),
            record(2, 10, finished("a", 1)),
        ];
        let summary = Summary::from_records("journal".into(), &records);
        assert_eq!(summary.hosts["worker"].failed, 1);
        assert!(summary.json().contains("\"p99\":500"), "{}", summary.json());
    }

    fn record(seq: u64, ts: u64, ev: Event) -> crate::state::Record {
        crate::state::Record { seq, ts, ev }
    }

    fn started(job: &str, host: &str, elapsed_us: u64) -> Event {
        Event::Started {
            job: job.into(),
            host: host.into(),
            elapsed_us,
        }
    }

    fn finished(job: &str, code: i32) -> Event {
        Event::Finished {
            job: job.into(),
            code,
        }
    }
}
