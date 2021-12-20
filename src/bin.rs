use std::error::Error;
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
    #[clap(short = 'd', long, default_value_t = 100)]
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

fn main() {
    let code = match main_exec() {
        Ok(_) => 0,
        Err(_) => -1,
    };
    std::process::exit(code);
}

fn main_exec() -> Result<(), Box<dyn Error>> {
    let args: Args = Args::parse();
    init_logging(&args);
    debug!("{:#?}", args);

    let (p_exec, p_args) = parse_cmd(args.pcmd, "playback");
    let (c_exec, c_args) = parse_cmd(args.ccmd, "capture");

    let mut c_cmd = CmdCfg::new(c_exec, c_args);
    let mut p_cmd = CmdCfg::new(p_exec, p_args);

    let (p_timer, p_canceller) = Timer::new2().unwrap();
    let (c_timer, c_canceller) = Timer::new2().unwrap();

    let (p_sender, p_recv) = unbounded();
    let (c_sender, c_recv) = unbounded();
    let p_debouncing = Arc::new(AtomicBool::new(false));
    let c_debouncing = Arc::new(AtomicBool::new(false));

    let mut p_thread_data = ExecData::new("Playback", p_timer, args.timeout, p_debouncing.clone(), p_recv.clone());
    let mut c_thread_data = ExecData::new("Capture", c_timer, args.timeout, c_debouncing.clone(), c_recv.clone());

    let mut p_loc_data = ExecLocData::new("Playback", p_canceller, p_debouncing, p_sender, p_recv);
    let mut c_loc_data = ExecLocData::new("Capture", c_canceller, c_debouncing, c_sender, c_recv);

    thread::Builder::new()
        .name("Playback Thread".to_string())
        .spawn(move || {
            executor::run_exec_thread(&mut p_thread_data, &mut p_cmd).unwrap();
        })?;
    thread::Builder::new()
        .name("Capture Thread".to_string())
        .spawn(move || {
            executor::run_exec_thread(&mut c_thread_data, &mut c_cmd).unwrap();
        })?;

    let c_srate_name = args.cctl.as_str();
    let p_srate_name = args.pctl.as_str();

    let cardname = args.gadget_name;
    let devname = format!("hw:{}", cardname).to_string();

    // initializing rate ctrls
    let h: HCtl = HCtl::new(&devname, false).unwrap();
    h.load().unwrap();
    let elem_crate = get_elem(c_srate_name, &h)
        .expect(format!("Capture rate ctl '{}' not found", c_srate_name).as_str());
    let crate_id = elem_crate.get_id().unwrap().get_numid();
    debug!("{} id {}", c_srate_name, crate_id);

    let elem_prate = get_elem(p_srate_name, &h)
        .expect(format!("Playback rate ctl '{}' not found", p_srate_name).as_str());
    let prate_id = elem_prate.get_id().unwrap().get_numid();
    debug!("{} id {}", p_srate_name, prate_id);

    // subscribing for blocking ctl.read
    let ctl = Ctl::new(&devname, false).unwrap();
    ctl.subscribe_events(true).unwrap();
    loop {
        let result = ctl.read();
        let event = result.unwrap().unwrap();

        // determining event control
        let numid = event.get_id().get_numid();
        trace!("Received event: elem num ID {}, index {}, mask {}", numid, event.get_id().get_index(), event.get_mask().0);
        if numid == crate_id {
            // capture rate
            send_new_rate(&elem_crate, &mut c_loc_data, args.show_timing)?;
        } else if numid == prate_id {
            // playback rate
            send_new_rate(&elem_prate, &mut p_loc_data, args.show_timing)?;
        }
    }
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

fn send_new_rate(elem: &Elem, data: &mut ExecLocData, show_timing: bool) -> Result<(), Box<dyn Error>> {
    let rate = read_value(Some(&elem)).unwrap() as usize;
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

fn get_elem<'a>(elemname: &str, h: &'a HCtl) -> Option<Elem<'a>> {
    let mut elid = ElemId::new(ElemIface::PCM);
    elid.set_device(0);
    elid.set_subdevice(0);
    elid.set_name(&CString::new(elemname).unwrap());
    let elem = h.find_elem(&elid);
    elem
}

fn read_value(elem: Option<&Elem>) -> Option<i32> {
    let value = elem.unwrap().read().unwrap();
    let rate = value.get_integer(0);
    rate
}