//! Jobs and CronJobs: status, durations, schedules with the next run, problems (failed jobs,
//! failing or missed cron runs), and the actions: run a CronJob now, suspend/resume it, delete a Job.

use anyhow::{Context, Result, anyhow, bail};
use jiff::{Timestamp, ToSpan, Zoned, tz::TimeZone};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use kube::api::{DeleteParams, Patch, PatchParams, PostParams, PropagationPolicy};
use kube::{Api, Client, Resource};
use serde_json::json;

use crate::model::Health;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    pub namespace: String,
    pub name: String,
    /// CronJob that created it.
    pub cronjob: Option<String>,
    /// "Running", "Complete", "Failed", "Suspended".
    pub status: String,
    pub active: i32,
    pub succeeded: i32,
    pub failed: i32,
    pub completions: i32,
    pub started: Option<Timestamp>,
    pub finished: Option<Timestamp>,
    /// Why it failed (condition message).
    pub message: String,
}

impl JobInfo {
    pub fn health(&self) -> Health {
        match self.status.as_str() {
            "Failed" => Health::Bad,
            "Running" if self.failed > 0 => Health::Warn,
            _ => Health::Ok,
        }
    }

    /// Run time so far, or until it finished.
    pub fn duration_secs(&self) -> Option<i64> {
        let start = self.started?;
        let end = self.finished.unwrap_or_else(Timestamp::now);
        Some(end.as_second() - start.as_second())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronJobInfo {
    pub namespace: String,
    pub name: String,
    pub schedule: String,
    pub time_zone: Option<String>,
    pub suspend: bool,
    pub active: usize,
    pub last_schedule: Option<Timestamp>,
    pub last_success: Option<Timestamp>,
    /// Status of the newest Job it created.
    pub last_status: Option<String>,
    pub next: Option<Timestamp>,
}

impl CronJobInfo {
    pub fn health(&self) -> Health {
        self.problems().first().map_or(Health::Ok, |(h, _)| *h)
    }

    pub fn problems(&self) -> Vec<(Health, String)> {
        let mut p = vec![];
        if self.last_status.as_deref() == Some("Failed") {
            p.push((Health::Bad, "the last run failed".into()));
        }
        if self.suspend {
            p.push((Health::Ok, "suspended: no new runs".into()));
        }
        // Missed: more than one full period passed since the last run should have happened.
        if !self.suspend
            && let (Some(last), Ok(c)) = (self.last_schedule, Cron::parse(&self.schedule))
            && let Some(after_last) = c.next_after(last, &self.zone())
            && let Some(following) = c.next_after(after_last, &self.zone())
            && Timestamp::now() > following
        {
            p.push((
                Health::Warn,
                format!("no run since {} — missed schedules (controller down, startingDeadlineSeconds?)", crate::ops::ago(last)),
            ));
        }
        if self.schedule.is_empty() || Cron::parse(&self.schedule).is_err() {
            p.push((Health::Warn, format!("schedule '{}' not understood here", self.schedule)));
        }
        p.sort_by_key(|(h, _)| *h);
        p
    }

    pub fn zone(&self) -> TimeZone {
        self.time_zone.as_deref().and_then(|z| TimeZone::get(z).ok()).unwrap_or(TimeZone::UTC)
    }
}

// ───────────────────────────── cron ─────────────────────────────

/// A standard 5-field cron schedule (minute hour day-of-month month day-of-week), with lists,
/// ranges, steps, month/day names and the @hourly/@daily/@weekly/@monthly/@yearly macros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minute: Vec<bool>,
    hour: Vec<bool>,
    dom: Vec<bool>,
    month: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

fn field(spec: &str, min: u32, max: u32, names: &[&str]) -> Result<Vec<bool>> {
    let mut set = vec![false; max as usize + 1];
    let value = |s: &str| -> Result<u32> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Ok(i as u32 + min);
        }
        s.parse::<u32>().map_err(|_| anyhow!("bad value '{s}'"))
    };
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| anyhow!("bad step '{s}'"))?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step 0");
        }
        let (lo, hi) = match range {
            "*" | "?" => (min, max),
            r => match r.split_once('-') {
                Some((a, b)) => (value(a)?, value(b)?),
                None if part.contains('/') => (value(r)?, max),
                None => (value(r)?, value(r)?),
            },
        };
        if lo < min || hi > max || lo > hi {
            bail!("'{part}' outside {min}-{max}");
        }
        let mut v = lo;
        while v <= hi {
            set[v as usize] = true;
            v += step;
        }
    }
    Ok(set)
}

impl Cron {
    pub fn parse(s: &str) -> Result<Cron> {
        let s = match s.trim() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        // A "TZ=Zone " / "CRON_TZ=Zone " prefix is handled by the caller through timeZone.
        let s = s.split_whitespace().filter(|f| !f.contains('=')).collect::<Vec<_>>();
        let [mi, h, dom, mo, dow] = s.as_slice() else { bail!("expected 5 fields") };
        const MONTHS: &[&str] = &["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
        const DAYS: &[&str] = &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
        let mut dow_set = field(dow, 0, 7, DAYS)?;
        if dow_set[7] {
            dow_set[0] = true; // 7 = Sunday too
        }
        Ok(Cron {
            minute: field(mi, 0, 59, &[])?,
            hour: field(h, 0, 23, &[])?,
            dom: field(dom, 1, 31, &[])?,
            month: field(mo, 1, 12, MONTHS)?,
            dow: dow_set,
            dom_any: *dom == "*" || *dom == "?",
            dow_any: *dow == "*" || *dow == "?",
        })
    }

    fn matches(&self, t: &Zoned) -> bool {
        let dom_ok = self.dom[t.day() as usize];
        let dow_ok = self.dow[t.weekday().to_sunday_zero_offset() as usize];
        // Cron's rule: when both day fields are restricted, either one matching is enough.
        let day = match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow_ok,
            (false, true) => dom_ok,
            (false, false) => dom_ok || dow_ok,
        };
        self.minute[t.minute() as usize] && self.hour[t.hour() as usize] && self.month[t.month() as usize] && day
    }

    /// First run strictly after `after`, searching up to ~4 years ahead.
    pub fn next_after(&self, after: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
        let mut t = after.to_zoned(tz.clone());
        t = t.with().second(0).subsec_nanosecond(0).build().ok()?.checked_add(1.minute()).ok()?;
        for _ in 0..(4 * 366 * 24 * 60) {
            if !self.month[t.month() as usize] {
                // Skip to the first minute of the next month.
                t = t.with().day(1).hour(0).minute(0).build().ok()?.checked_add(1.month()).ok()?;
                continue;
            }
            if self.matches(&t) {
                return Some(t.timestamp());
            }
            t = t.checked_add(1.minute()).ok()?;
        }
        None
    }
}

// ───────────────────────────── from objects ─────────────────────────────

pub fn job_info(j: &Job) -> JobInfo {
    let st = j.status.clone().unwrap_or_default();
    let spec = j.spec.clone().unwrap_or_default();
    let conditions = st.conditions.clone().unwrap_or_default();
    let cond = |t: &str| conditions.iter().find(|c| c.type_ == t && c.status == "True");
    let status = if cond("Failed").is_some() {
        "Failed"
    } else if cond("Complete").is_some() {
        "Complete"
    } else if spec.suspend == Some(true) {
        "Suspended"
    } else {
        "Running"
    };
    JobInfo {
        namespace: j.metadata.namespace.clone().unwrap_or_default(),
        name: j.metadata.name.clone().unwrap_or_default(),
        cronjob: j.metadata.owner_references.iter().flatten().find(|o| o.kind == "CronJob").map(|o| o.name.clone()),
        status: status.into(),
        active: st.active.unwrap_or(0),
        succeeded: st.succeeded.unwrap_or(0),
        failed: st.failed.unwrap_or(0),
        completions: spec.completions.unwrap_or(1),
        started: st.start_time.map(|t| t.0),
        finished: st.completion_time.map(|t| t.0).or_else(|| cond("Failed").and_then(|c| c.last_transition_time.clone()).map(|t| t.0)),
        message: cond("Failed").and_then(|c| c.message.clone()).unwrap_or_default(),
    }
}

pub fn cronjob_info(c: &CronJob, jobs: &[JobInfo]) -> CronJobInfo {
    let spec = c.spec.clone();
    let st = c.status.clone().unwrap_or_default();
    let ns = c.metadata.namespace.clone().unwrap_or_default();
    let name = c.metadata.name.clone().unwrap_or_default();
    let newest = jobs
        .iter()
        .filter(|j| j.namespace == ns && j.cronjob.as_deref() == Some(&name))
        .max_by_key(|j| j.started.map(|t| t.as_second()).unwrap_or(0));
    let mut info = CronJobInfo {
        namespace: ns,
        name,
        schedule: spec.schedule.clone(),
        time_zone: spec.time_zone.clone(),
        suspend: spec.suspend.unwrap_or(false),
        active: st.active.map_or(0, |a| a.len()),
        last_schedule: st.last_schedule_time.map(|t| t.0),
        last_success: st.last_successful_time.map(|t| t.0),
        last_status: newest.map(|j| j.status.clone()),
        next: None,
    };
    if !info.suspend {
        info.next = Cron::parse(&info.schedule).ok().and_then(|cr| cr.next_after(Timestamp::now(), &info.zone()));
    }
    info
}

// ───────────────────────────── actions ─────────────────────────────

/// `kubectl create job --from=cronjob/NAME`: a Job from the CronJob's template, owned by it.
pub async fn run_now(client: &Client, namespace: &str, cronjob: &str, dry_run: bool) -> Result<String> {
    let cj =
        Api::<CronJob>::namespaced(client.clone(), namespace).get(cronjob).await.with_context(|| format!("reading cronjob {cronjob}"))?;
    let template = cj.spec.job_template.clone();
    let stamp = Timestamp::now().as_second() % 1_000_000;
    let name = format!("{}-manual-{stamp}", cronjob.chars().take(40).collect::<String>());
    let mut meta = template.metadata.clone().unwrap_or_default();
    meta.name = Some(name.clone());
    meta.namespace = Some(namespace.to_string());
    meta.annotations.get_or_insert_with(Default::default).insert("cronjob.kubernetes.io/instantiate".into(), "manual".into());
    meta.owner_references = Some(vec![cj.controller_owner_ref(&()).ok_or_else(|| anyhow!("cronjob has no uid"))?]);
    let job = Job { metadata: meta, spec: template.spec, status: None };
    let pp = PostParams { dry_run, field_manager: Some("k8s-galaxy".into()) };
    Api::<Job>::namespaced(client.clone(), namespace).create(&pp, &job).await.with_context(|| format!("creating job {name}"))?;
    Ok(name)
}

pub async fn suspend(client: &Client, namespace: &str, cronjob: &str, on: bool, dry_run: bool) -> Result<()> {
    let pp = PatchParams { field_manager: Some("k8s-galaxy".into()), dry_run, ..Default::default() };
    Api::<CronJob>::namespaced(client.clone(), namespace).patch(cronjob, &pp, &Patch::Merge(json!({"spec": {"suspend": on}}))).await?;
    Ok(())
}

pub async fn delete_job(client: &Client, namespace: &str, job: &str, dry_run: bool) -> Result<()> {
    let dp = DeleteParams { dry_run, propagation_policy: Some(PropagationPolicy::Background), ..Default::default() };
    Api::<Job>::namespaced(client.clone(), namespace).delete(job, &dp).await?;
    Ok(())
}

/// "in 3h 20m", "in 5m".
pub fn until(t: Timestamp) -> String {
    let s = (t.as_second() - Timestamp::now().as_second()).max(0);
    match s {
        s if s < 3600 => format!("in {}m", s / 60),
        s if s < 86_400 => format!("in {}h {}m", s / 3600, s % 3600 / 60),
        s => format!("in {}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn next_runs() {
        let utc = TimeZone::UTC;
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(c.next_after(at("2026-10-19T10:07:30Z"), &utc), Some(at("2026-10-19T10:15:00Z")));
        let c = Cron::parse("30 2 * * MON-FRI").unwrap();
        // 2026-10-24 is a Saturday: next is Monday the 26th.
        assert_eq!(c.next_after(at("2026-10-23T03:00:00Z"), &utc), Some(at("2026-10-26T02:30:00Z")));
        let c = Cron::parse("@monthly").unwrap();
        assert_eq!(c.next_after(at("2026-10-19T00:00:00Z"), &utc), Some(at("2026-11-01T00:00:00Z")));
        let c = Cron::parse("0 9 1,15 * *").unwrap();
        assert_eq!(c.next_after(at("2026-10-02T00:00:00Z"), &utc), Some(at("2026-10-15T09:00:00Z")));
        let c = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(c.next_after(at("2026-03-01T00:00:00Z"), &utc), Some(at("2028-02-29T00:00:00Z")));
        let warsaw = TimeZone::get("Europe/Warsaw").unwrap();
        let c = Cron::parse("0 3 * * *").unwrap();
        // 03:00 in Warsaw (CEST, UTC+2) on 2026-10-20 is 01:00 UTC.
        assert_eq!(c.next_after(at("2026-10-19T12:00:00Z"), &warsaw), Some(at("2026-10-20T01:00:00Z")));
        assert!(Cron::parse("61 * * * *").is_err());
        assert!(Cron::parse("* * *").is_err());
    }
}
