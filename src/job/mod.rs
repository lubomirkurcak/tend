use crate::colors::TendColors;
use anyhow::Result;
use clap::ValueEnum;
use folktime::Folktime;
use prettytable::{format, row, Table};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc::Receiver,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    pub group: String,
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    #[serde(default)]
    pub restart: JobRestartBehavior,
    #[serde(default)]
    pub restart_strategy: JobRestartStrategy,
    #[serde(default)]
    pub event_hooks: HashMap<String, JobEventHook>,
    #[serde(default)]
    pub template: Option<JobTemplate>,
}

#[derive(Copy, Clone, Debug, ValueEnum, Serialize, Deserialize)]
pub enum JobTemplate {
    PortForward,
}

enum JobControlFlow {
    Nothing,
    RestartCommand,
    StopJob,
}

impl Job {
    fn stdout_line_callback(&self, line: &str) -> JobControlFlow {
        for hook in self.event_hooks.values() {
            let JobEventHook {
                event: JobEvent::DetectSubstring { stream, contains },
                action,
            } = hook;

            let detection = match stream {
                Stream::Stdout => line.contains(contains),
                Stream::Stderr => false,
                Stream::Any => line.contains(contains),
            };

            if detection {
                match action {
                    JobAction::Restart => return JobControlFlow::RestartCommand,
                    JobAction::Stop => return JobControlFlow::StopJob,
                }
            }
        }

        JobControlFlow::Nothing
    }

    fn stderr_line_callback(&self, line: &str) -> JobControlFlow {
        for hook in self.event_hooks.values() {
            let JobEventHook {
                event: JobEvent::DetectSubstring { stream, contains },
                action,
            } = hook;

            let detection = match stream {
                Stream::Stdout => false,
                Stream::Stderr => line.contains(contains),
                Stream::Any => line.contains(contains),
            };

            if detection {
                match action {
                    JobAction::Restart => return JobControlFlow::RestartCommand,
                    JobAction::Stop => return JobControlFlow::StopJob,
                }
            }
        }

        JobControlFlow::Nothing
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, clap::ValueEnum, Copy, PartialEq, Eq)]
pub enum JobRestartBehavior {
    #[default]
    Always,
    OnSuccess,
    OnFailure,
    Never,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, clap::ValueEnum, Copy, PartialEq)]
pub enum JobRestartStrategy {
    Immediate,
    #[default]
    ExponentialBackoff,
}

impl JobRestartStrategy {
    pub fn delay_seconds(&self, restarts: u64) -> u64 {
        match self {
            JobRestartStrategy::Immediate => 0,
            JobRestartStrategy::ExponentialBackoff => [0, 0, 0, 1, 2, 4, 8, 15, 30]
                .get(restarts as usize)
                .copied()
                .unwrap_or(60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ValueEnum, Default)]
pub enum Stream {
    Stdout,
    Stderr,
    #[default]
    Any,
}

/// TODO: Rework [Job::restart] to use this instead of [JobRestartStrategy]
#[derive(Debug, Clone, Serialize, Deserialize, clap::Parser)]
pub enum JobEvent {
    // FinishedSuccess,
    // FinishedFailure,
    DetectSubstring { stream: Stream, contains: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, ValueEnum)]
pub enum JobAction {
    Restart,
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEventHook {
    pub event: JobEvent,
    pub action: JobAction,
}

pub enum JobFilter {
    All {
        exclude: Vec<String>,
    },
    Subset {
        jobs: Vec<String>,
        groups: Vec<String>,
        exclude: Vec<String>,
    },
}

impl JobFilter {
    pub fn matches(&self, job: &Job) -> bool {
        match self {
            JobFilter::All { exclude } => !exclude.contains(&job.name),
            JobFilter::Subset {
                groups,
                jobs,
                exclude,
            } => {
                if exclude.contains(&job.name) {
                    return false;
                }

                groups.contains(&job.group) || jobs.contains(&job.name)
            }
        }
    }
}

impl Job {
    fn jobs_dir() -> Result<PathBuf> {
        let home = dirs_next::home_dir().ok_or(anyhow::anyhow!("Could not find home directory"))?;
        let jobs = home.join(".tend").join("jobs");
        std::fs::create_dir_all(&jobs)?;
        Ok(jobs)
    }

    pub fn save(&self, overwrite: bool) -> Result<()> {
        let jobs = Self::jobs_dir()?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(overwrite)
            .create_new(!overwrite)
            .open(jobs.join(&self.name))?;
        // serde_json::to_writer(file, self)?;
        serde_json::to_writer_pretty(file, self)?;

        Ok(())
    }

    pub fn load(name: &str) -> Result<Self> {
        let jobs = Self::jobs_dir()?;
        let file = std::fs::File::open(jobs.join(name))?;
        let job: Job = serde_json::from_reader(file)?;

        Ok(job)
    }

    pub fn delete(&self) -> Result<()> {
        let jobs = Self::jobs_dir()?;
        std::fs::remove_file(jobs.join(&self.name))?;

        Ok(())
    }

    pub fn iterate_jobs_filtered<F>(mut f: F, filter: &JobFilter) -> Result<()>
    where
        F: FnMut(Job),
    {
        let jobs = Self::jobs_dir()?;
        for entry in std::fs::read_dir(jobs)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let job: Job = serde_json::from_reader(std::fs::File::open(&path)?)?;
                if filter.matches(&job) {
                    f(job);
                }
            }
        }

        Ok(())
    }

    pub fn list(job_filter: JobFilter) -> Result<()> {
        let jobs = Self::jobs_dir()?;
        let mut table = Table::new();
        table.set_format(*format::consts::FORMAT_NO_LINESEP_WITH_TITLE);
        //table.set_format(*format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);

        table.set_titles(row![
            "Job",
            "Program",
            "Args",
            "Working Directory",
            "Restart",
            "Group"
        ]);

        for entry in std::fs::read_dir(jobs)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let job: Job = serde_json::from_reader(std::fs::File::open(&path)?)?;
                if !job_filter.matches(&job) {
                    continue;
                }

                table.add_row(row![
                    job.name.job(),
                    job.program.program(),
                    job.args.join(" "),
                    job.working_directory.display(),
                    job.restart_behaviour(),
                    job.group,
                ]);
            }
        }

        if table.is_empty() {
            println!("No jobs found");
        } else {
            table.printstd();
        }

        Ok(())
    }
}

impl Job {
    fn create_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.current_dir(&self.working_directory);
        command.args(&self.args);
        command
    }

    pub async fn create_repeated_process(self, mut rx: Receiver<()>, verbose: bool) -> Result<()> {
        let mut command = self.create_command();

        let mut successes = 0;
        let mut failures = 0;

        'job: loop {
            if verbose {
                println!("{} starting", self.name.job(),);
            }
            let start_time = std::time::Instant::now();
            let mut process = command
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;

            let mut stdout = BufReader::new(
                process
                    .stdout
                    .take()
                    .ok_or(anyhow::anyhow!("Could not get stdout"))?,
            )
            .lines();

            let mut stderr = BufReader::new(
                process
                    .stderr
                    .take()
                    .ok_or(anyhow::anyhow!("Could not get stderr"))?,
            )
            .lines();

            'process: loop {
                tokio::select! {
                    stdout_line = stdout.next_line() => {
                        if let Some(line) = stdout_line? {
                            println!("{}{}", format!("{}: ", self.name).job(), line);

                            match self.stdout_line_callback(&line) {
                                JobControlFlow::Nothing => (),
                                JobControlFlow::RestartCommand => {
                                    println!("{} restarting", self.name.job());
                                    process.kill().await?;
                                    break 'process;
                                },
                                JobControlFlow::StopJob => {
                                    println!("{} stopping", self.name.job());
                                    process.kill().await?;
                                    break 'job;
                                },
                            };
                        }
                        continue 'process;
                    }
                    stderr_line = stderr.next_line() => {
                        if let Some(line) = stderr_line? {
                            println!("{}{}{}{}", self.name.job(), " (stderr)".failure(), ": ".job(), line);

                            match self.stderr_line_callback(&line) {
                                JobControlFlow::Nothing => (),
                                JobControlFlow::RestartCommand => {
                                    println!("{} restarting", self.name.job());
                                    process.kill().await?;
                                    break 'process;
                                },
                                JobControlFlow::StopJob => {
                                    println!("{} stopping", self.name.job());
                                    process.kill().await?;
                                    break 'job;
                                },
                            };
                        }
                        continue 'process;
                    }
                    a = process.wait() => {
                        let end_time = std::time::Instant::now();
                        let job_duration = end_time.duration_since(start_time);
                        match a {
                            Ok(status) => {
                                if status.success() {
                                    successes += 1;
                                    println!(
                                        "{} process finished indicating {} after running for {}",
                                        self.name.job(),
                                        "success".success(),
                                        Folktime::duration(job_duration).to_string().time_value(),
                                    );
                                    if self.restart_on_success() {
                                        let delay_seconds = self.restart_strategy.delay_seconds(successes);
                                        if delay_seconds != 0 {
                                            println!("{} restarting in {} seconds", self.name.job(), delay_seconds);
                                            tokio::time::sleep(tokio::time::Duration::from_secs(delay_seconds)).await;
                                        }
                                    } else {
                                        println!("{} stopping", self.name.job());
                                        break 'job;
                                    }
                                } else {
                                    failures += 1;
                                    println!(
                                        "{} process finished indicating {} after running for {}",
                                        self.name.job(),
                                        "failure".failure(),
                                        Folktime::duration(job_duration).to_string().time_value(),
                                    );
                                    if self.restart_on_failure() {
                                        let delay_seconds = self.restart_strategy.delay_seconds(failures);
                                        if delay_seconds != 0 {
                                            println!("{} restarting in {} seconds", self.name.job(), delay_seconds);
                                            tokio::time::sleep(tokio::time::Duration::from_secs(delay_seconds)).await;
                                        }
                                    } else {
                                        println!("{} stopping", self.name.job());
                                        break 'job;
                                    }
                                }
                            }
                            Err(e) => {
                                println!(
                                    "{} could not be awaited ({:?})",
                                    self.name.job(),
                                    e.to_string().failure()
                                );
                            }
                        }
                    }
                    _ = rx.recv() => {
                        // println!("killing process of {}", self.name.job());
                        process.kill().await?;
                        break 'job;
                    }
                }
                break 'process;
            }
        }

        Ok(())
    }
}

impl Job {
    pub fn restart_on_success(&self) -> bool {
        match self.restart {
            JobRestartBehavior::Always | JobRestartBehavior::OnSuccess => true,
            JobRestartBehavior::OnFailure | JobRestartBehavior::Never => false,
        }
    }

    pub fn restart_on_failure(&self) -> bool {
        match self.restart {
            JobRestartBehavior::Always | JobRestartBehavior::OnFailure => true,
            JobRestartBehavior::OnSuccess | JobRestartBehavior::Never => false,
        }
    }

    pub fn restart_behaviour(&self) -> &'static str {
        match self.restart {
            JobRestartBehavior::Always => "always",
            JobRestartBehavior::OnSuccess => "on success",
            JobRestartBehavior::OnFailure => "on failure",
            JobRestartBehavior::Never => "never",
        }
    }
}
