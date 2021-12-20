use std::io;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use cancellable_timer::Timer;
use crossbeam_channel::Receiver;
use log::{debug, error, trace, warn};

use crate::Msg;

pub struct ExecData {
    dir: String,
    // running exec process
    child: Option<Child>,
    // debouncing timer
    timer: Timer,
    // debouncing timeout (0 = no debouncing)
    timeout: usize,
    // is currently in debouncing wait
    debouncing_now: Arc<AtomicBool>,
    // value reported by the Rate ctl
    rate: usize,
    // to receive new rate
    recv: Receiver<Msg>,
}

impl ExecData {
    pub fn new(dir: &str, timer: Timer, timeout: usize, debouncing: Arc<AtomicBool>, recv: Receiver<Msg>) -> Self {
        ExecData {
            dir: dir.to_string(),
            child: None,
            rate: 0,
            timer,
            timeout,
            debouncing_now: debouncing,
            recv,
        }
    }
}

#[derive(Debug)]
pub struct CmdCfg {
    exec: String,
    args: Vec<String>,
}

impl CmdCfg {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self {
            exec: program,
            args: args,
        }
    }
}

pub fn run_exec_thread(data: &mut ExecData, cmd: &mut CmdCfg) -> Result<()> {
    loop {
        match data.recv.recv() {
            Ok(msg) => {
                match msg {
                    Msg::StartExec(rate) => handle_new_rate(rate, data, cmd)?,
                    Msg::StopExec => handle_new_rate(0, data, cmd)?,
                    Msg::Quit => {
                        debug!("Ordered to quit");
                        kill_running_child(data)?;
                        break;
                    }
                }
            }
            Err(err) => {
                error!("Message channel error: {}", err);
                break;
            }
        }
    }
    Ok(())
}

fn handle_new_rate(rate: usize, data: &mut ExecData, cmd: &mut CmdCfg) -> Result<()> {
    debug!("{}: Received new rate: {}", data.dir, rate);
    let (do_kill, do_start) = decide_kill_run(data.rate, rate);
    if do_kill {
        kill_running_child(data)?;
    }
    if do_start {
        // delaying to debounce
        if data.timeout > 0 {
            trace!("{}: Debouncing - delaying start for {}ms", data.dir, data.timeout);
            data.debouncing_now.store(true, Ordering::SeqCst);
            match data.timer.sleep(Duration::from_millis(data.timeout as u64)) {
                Ok(_) => {
                    trace!("{}: Debouncing elapsed, starting exec", data.dir);
                    data.child = start_child(cmd, rate);
                }
                Err(_) => {
                    trace!("{}: Debouncing cancelled, not starting exec", data.dir);
                }
            }
            data.debouncing_now.store(false, Ordering::SeqCst);
        } else {
            trace!("{}: Starting exec without debouncing", data.dir);
            data.child = start_child(cmd, rate);
        }
    }
    data.rate = rate;
    Ok(())
}

// rate 0 = stop
fn decide_kill_run(last_rate: usize, rate: usize) -> (bool, bool) {
    let do_kill = /* any change in rate, unless it was zero */ last_rate > 0 && last_rate != rate;
    let do_run = /* should run */ rate > 0 && (/* new start */  last_rate == 0 || /* restart */ do_kill);
    (do_kill, do_run)
}

fn kill_running_child(data: &mut ExecData) -> Result<(), std::io::Error> {
    let option = data.child.as_mut();
    if option.is_some() {
        debug!("{}: killing exec", data.dir);
        let child: &mut Child = option.unwrap();
        if let Err(err) = kill_child(child) {
            match (err).kind() {
                // no problem
                io::ErrorKind::InvalidInput => debug!("exec has already finished"),
                _ => {
                    // some other error, problem
                    warn!("Cmd failed, error: {}", err);
                    return Err(err);
                }
            }
        }
        data.child = None;
    }
    Ok(())
}

fn kill_child(child: &mut Child) -> Result<(), std::io::Error> {
    child.kill()?;
    child.wait()?;
    Ok(())
}

fn start_child(cmd: &mut CmdCfg, rate: usize) -> Option<Child> {
    // replacing RATE value in command args
    let final_args: Vec<String> = cmd.args.iter().map(|s| {
        if s.contains("{R}") {
            s.replace("{R}", rate.to_string().as_str())
        } else {
            s.to_string()
        }
    }).collect();
    let child = match Command::new(&cmd.exec)
        .args(&final_args)
        .spawn() {
        Ok(res) => Some(res),
        Err(err) => {
            warn!("Cmd failed, error: {}", err);
            None
        }
    };
    debug!("Started: exec {}, args: {:#?}", cmd.exec, final_args);
    child
}