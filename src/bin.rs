use std::ffi::CString;
use std::fmt::Debug;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use alsa::Ctl;
use alsa::ctl::{ElemId, ElemIface};
use alsa::hctl::{Elem, HCtl};
use anyhow::{anyhow, Result};
use cancellable_timer::{Canceller, Timer};
use clap::Parser;
use crossbeam_channel::{Receiver, Sender, unbounded};
use env_logger::Builder;
use log::{debug, info, LevelFilter, trace};

use executor::{CmdCfg, ExecData};

mod executor;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    /// Debouncing timeout in ms, 0 = no debouncing
    #[clap(short = 'd', long, default_value_t = 50)]
    timeout: usize,

    /// Verbose (-v = debug, -vv = trace)
    #[clap(short, long, parse(from_occurrences))]
    verbose: u8,

    /// Show start/stop timing
    #[clap(short = 't', long)]
    show_timing: bool,

    /// Gadget card name
    #[clap(short = 'g', long, default_value = "UAC2Gadget")]
    gadget_name: String,

    /// Playback Rate ctl name
    #[clap(short = 'p', long, default_value = "Playback Rate")]
    pctl: String,

    /// Capture Rate ctl name
    #[clap(short = 'c', long, default_value = "Capture Rate")]
    cctl: String,

    /// Playback command ({R} replaced with real rate)
    #[clap(short = 'x', long, default_value = "alsaloop -vv -r {R} --latency=1000 -f S32_LE -S playshift -C hw:Loopback,1 -P hw:UAC2Gadget")]
    pcmd: String,

    /// Capture command ({R} replaced with real rate)
    #[clap(short = 'y', long, default_value = "alsaloop -vv -r {R} --latency=1000 -f S32_LE -S captshift -C hw:UAC2Gadget -P hw:Loopback,1")]
    ccmd: String,
}

// messages sent to exec threads
pub enum Msg {
    // stop exec
    StopExec,
    // start with rate
    StartExec(usize),
    // stop the thread
    Quit,
}

struct ExecLocData {
    dir: String,
    canceller: Canceller,
    debouncing_now: Arc<AtomicBool>,
    sender: Sender<Msg>,
    draining_recv: Receiver<Msg>,
    last_start: Option<Instant>,
}

impl ExecLocData {
    pub fn new(dir: &str, canceller: Canceller, debouncing_now: Arc<AtomicBool>, sender: Sender<Msg>, recv: Receiver<Msg>) -> Self {
        ExecLocData {
            dir: dir.to_string(),
            canceller,
            debouncing_now,
            sender,
            draining_recv: recv,
            last_start: None,
        }
    }
}

struct CtlData<'a> {
    elem: Elem<'a>,
    numid: u32,
}

fn main() -> Result<()> {
    let args: Args = Args::parse();
    init_logging(&args);
    debug!("{:#?}", args);

    let devname = format!("hw:{}", args.gadget_name).to_string();

    // initializing rate ctrls and corresponding executors
    let h = HCtl::new(&devname, false)?;
    h.load()?;

    let c_ctl_data = get_ctl_data(&h, args.cctl.as_str())?;
    let mut c_exec_data = match c_ctl_data {
        Some(_) => {
            trace!("Ctl '{}' found, will start capture exec", args.cctl);
            Some(init_executor("Capture", args.ccmd, args.timeout)?)
        }
        None => {
            info!("Ctl '{}' not found, will not start capture exec", args.cctl);
            None
        }
    };

    let p_ctl_data = get_ctl_data(&h, args.pctl.as_str())?;
    let mut p_exec_data = match p_ctl_data {
        Some(_) => {
            trace!("Ctl '{}' found, will start playback exec", args.pctl);
            Some(init_executor("Playback", args.pcmd, args.timeout)?)
        }
        None => {
            info!("Ctl '{}' not found, will not start playback exec", args.pctl);
            None
        }
    };

    if c_ctl_data.is_none() && p_ctl_data.is_none() {
        return Err(anyhow!("Neither capture nor playback rate controls found, exiting"));
    }

    // subscribing for blocking ctl.read
    let ctl = Ctl::new(&devname, false)?;
    ctl.subscribe_events(true)?;
    loop {
        let event = ctl.read()?.unwrap();
        // determining event control
        let numid = event.get_id().get_numid();
        trace!("Received event: elem num ID {}, index {}, mask {}", numid, event.get_id().get_index(), event.get_mask().0);
        if fits_numid(&c_ctl_data, numid) {
            // capture rate
            send_new_rate(&c_ctl_data.as_ref().unwrap().elem, c_exec_data.as_mut().unwrap(), args.show_timing)?;
        } else if fits_numid(&p_ctl_data, numid) {
            // playback rate
            send_new_rate(&p_ctl_data.as_ref().unwrap().elem, p_exec_data.as_mut().unwrap(), args.show_timing)?;
        }
    }
}

#[inline]
fn fits_numid(ctl_data: &Option<CtlData>, numid: u32) -> bool {
    ctl_data.is_some() && ctl_data.as_ref().unwrap().numid == numid
}

fn init_executor(dir: &str, cmd: String, timeout: usize) -> Result<ExecLocData> {
    let (exec, c_args) = parse_cmd(cmd, dir);
    let mut cmd_cfg = CmdCfg::new(exec, c_args);
    let (timer, canceller) = Timer::new2()?;
    let (sender, recv) = unbounded();
    let debouncing = Arc::new(AtomicBool::new(false));
    let mut thread_data = ExecData::new(dir, timer, timeout, debouncing.clone(), recv.clone());
    thread::Builder::new()
        .name(format!("{} Thread", dir))
        .spawn(move || {
            executor::run_exec_thread(&mut thread_data, &mut cmd_cfg).unwrap();
        })?;
    let data = ExecLocData::new(dir, canceller, debouncing, sender, recv);
    Ok(data)
}

fn get_ctl_data<'a>(h: &'a HCtl, elem_name: &'a str) -> Result<Option<CtlData<'a>>> {
    return match get_elem(elem_name, &h)? {
        Some(elem) => {
            let numid = elem.get_id()?.get_numid();
            debug!("{} id {}", elem_name, numid);
            Ok(Some(CtlData { elem, numid }))
        }
        None => Ok(None)
    };
}

fn parse_cmd(cmd: String, dir: &str) -> (String, Vec<String>) {
    let mut split = cmd.split_whitespace();
    let exec = split.next().expect(format!("Missing {} executable", dir).as_str());
    let args = split.map(str::to_string).collect();

    debug!("{} exec: {:#?}", dir, exec);
    debug!("{} args: {:#?}", dir, args);
    (exec.to_string(), args)
}

fn init_logging(args: &Args) {
    Builder::new()
        .format(|buf, record| {
            writeln!(buf, "{}", record.args())
        })
        .filter(None, match args.verbose {
            0 => LevelFilter::Info,
            1 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        })
        .init();
}

fn send_new_rate(elem: &Elem, data: &mut ExecLocData, show_timing: bool) -> Result<()> {
    let rate = read_value(&elem)?.unwrap() as usize;
    debug!("{}: New rate value: {}", data.dir, rate);
    if show_timing {
        print_timing(data, rate)
    }

    if rate == 0 {
        // requesting STOP
        // draining the channel for possible unconsumed requests
        let drained_cnt = data.draining_recv.try_iter().count();
        trace!("{}: Drained {} messages", data.dir, drained_cnt);
        if data.debouncing_now.load(Ordering::SeqCst) {
            // cancelling the debouncing timer in the exec thread
            debug!("{}: Cancelling debounce wait", data.dir);
            data.canceller.cancel()?;
        }
        data.sender.send(Msg::StopExec)?;
    } else {
        // sending the required rate
        data.sender.send(Msg::StartExec(rate))?;
    }
    Ok(())
}

fn print_timing(data: &mut ExecLocData, rate: usize) {
    if rate == 0 && data.last_start.is_some() {
        let duration = Instant::now() - data.last_start.unwrap();
        info!("{}: STOP received after {} ms", data.dir, duration.as_millis());
    }
    if rate > 0 {
        data.last_start = Some(Instant::now());
    }
}

fn get_elem<'a>(elemname: &str, h: &'a HCtl) -> Result<Option<Elem<'a>>> {
    let mut elid = ElemId::new(ElemIface::PCM);
    elid.set_device(0);
    elid.set_subdevice(0);
    elid.set_name(&CString::new(elemname)?);
    let elem = h.find_elem(&elid);
    Ok(elem)
}

fn read_value(elem: &Elem) -> Result<Option<i32>> {
    let value = elem.read()?;
    let rate = value.get_integer(0);
    Ok(rate)
}