# Controller for USB Audio Gadget
The controller subscribes to Playback/Capture Rate alsa controls defined by the gadget alsa device and starts/stops appropriate playback/capture processes on the gadget side.

## Communicating from the Gadget
When no playback/capture runs on the USB host side or the USB cable is disconnected, the respective Playback/Capture Rate controls report 0. When playback/capture is started, the controls report the actual samplerate in Hz. The default control names are the names hard-coded in the kernel audio gadget code, configurable with params `-p/--pctl` and `-c/--cctl`.

## Playback/Capture Processes on the Gadget Side
The process commands are specified by params `-x/--pcmd` resp. `-y/--ccmd`. The controller executes the commands directly without any shell. Every occurence of string `{R}` is replaced with current samplerate in Hz, as reported by the corresponding alsa control.
The default commands run alsaloop to Loopback devices.
### Capture
The direction notation is from the gadget view (same as the gaudio alsa notation). **Capture means FROM the gadget alsa device** (i.e. playback from the USB host view).
### Playback
**Playback means TO the gadget alsa device** (i.e. capture from the USB host view)

## Debouncing
USB audio drivers of USB hosts test functionality of the USB device during enumeration. Also pulseaudio tries to open alsa devices. In order to avoid bounced starting/killing the gadget-side processes, the controller implements a debouncer, delaying start of the respective process by a timeout in param `-d/--timeout`. Value 0 disables the debouncing. With parameter `-t/--show-timing` the controller measures and reports the time between start and stop events, allowing to set debouncing timeout optimal for the specific usage. The optimal value is slightly larger than the maximum reported stop-start time when plugging the USB cable in. My linux host enumeration bounces take around 25ms, therefore the default value is set to 50 ms. That means that at every playback/capture start on the USB host the first 50ms of data will be lost, but the controller will not run any process on the gadget side during enumeration.

## Requirements
* If alsaloop is used, the version in alsa-utils 1.2.6 supports Capture/Playback Pitch gadget feedback controls.
* All required patches for the audio gadget have not been submitted yet, subject to change soon.

## Installation
1. Installing latest stable Rust with [rustup](https://www.rust-lang.org/tools/install).
2. `cd gadget_ctl`
3. `cargo build --release`
4. The binary is compiled to `target/release/gadget_ctl`

## Running
Alsaloop 1.2.6 is not in raspbian yet, therefore exec commands must be specified appropriately:
```
target/release/gaudio_ctl -vv -t -g UAC2Gadget -p "Playback Rate" -c "Capture Rate" -d 40 -x "/home/pi/work/alsa-utils/alsaloop/alsaloop -vvvv -r {R} -S playshift -f S32_LE -C hw:Loopback,1 -P hw:UAC2Gadget  --latency=10000" -y "/home/pi/work/alsa-utils/alsaloop/alsaloop -vvvv -r {R} -S captshift -f S32_LE -P hw:Loopback,1 -C hw:UAC2Gadget --latency=10000"
```
