use std::{ops::Range, os::unix::process::CommandExt, process::Command, sync::mpsc};

use nix::{
    sys::{
        ptrace::{self},
        signal::Signal,
        wait::{waitpid, WaitStatus},
    },
    unistd::Pid,
};
use owo_colors::OwoColorize;
use tracing::{info, trace, warn};

use crate::{MapGuard, MemState, TraceePayload};

pub(crate) fn run(tx: mpsc::SyncSender<TraceePayload>) {
    Tracee::new(tx).unwrap().run().unwrap();
}

struct Tracee {
    tx: mpsc::SyncSender<TraceePayload>,
    pid: Pid,
    heap_range: Option<Range<usize>>,
}

struct Mapped {
    range: Range<usize>,
    resident: MemState,
}

impl Tracee {
    fn new(tx: mpsc::SyncSender<TraceePayload>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = std::env::args();
        // skip our own name
        args.next().unwrap();

        let mut cmd = Command::new(args.next().unwrap());
        for arg in args {
            cmd.arg(arg);
        }
        cmd.env("LD_PRELOAD", "target/release/libmevi_preload.so");
        unsafe {
            cmd.pre_exec(|| {
                ptrace::traceme()?;
                Ok(())
            });
        }

        let child = cmd.spawn().unwrap();

        let pid = Pid::from_raw(child.id() as _);
        std::mem::forget(child);

        let res = waitpid(pid, None)?;
        trace!("first waitpid: {res:?}");

        Ok(Self {
            tx,
            pid,
            heap_range: None,
        })
    }

    fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            self.syscall_step()?;
            self.syscall_step()?;

            if let Some(Mapped { range, resident }) = self.on_sys_exit()? {
                let (tx, rx) = mpsc::channel();
                self.tx
                    .send(TraceePayload::Map {
                        range,
                        resident,
                        _guard: MapGuard { _inner: Some(tx) },
                    })
                    .unwrap();

                // this will fail, because it's been dropped. but it'll
                // wait until it's dropped, which is what we want
                _ = rx.recv();
            }
        }
    }

    fn syscall_step(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            ptrace::syscall(self.pid, None)?;
            let wait_status = waitpid(self.pid, None)?;
            trace!("wait_status: {:?}", wait_status.yellow());
            match wait_status {
                WaitStatus::Stopped(_, Signal::SIGTRAP) => break Ok(()),
                WaitStatus::Stopped(_, _other_sig) => {
                    warn!("caught other sig: {_other_sig}");
                    continue;
                }
                WaitStatus::Exited(_, status) => {
                    info!("Child exited with status {status}");
                    std::process::exit(status);
                }
                _ => continue,
            }
        }
    }

    fn on_sys_exit(&mut self) -> Result<Option<Mapped>, Box<dyn std::error::Error>> {
        let regs = ptrace::getregs(self.pid)?;
        trace!("on sys_exit: {regs:?}");
        let ret = regs.rax as usize;

        match regs.orig_rax as i64 {
            libc::SYS_mmap => {
                let fd = regs.r8 as i32;
                let addr_in = regs.rdi;
                let len = regs.rsi as usize;

                if fd == -1 && addr_in == 0 {
                    return Ok(Some(Mapped {
                        range: ret..ret + len,
                        resident: MemState::NotResident,
                    }));
                }
            }
            libc::SYS_brk => {
                if regs.rdi == 0 {
                    // just a query: remember the top of the heap
                    if self.heap_range.is_none() {
                        self.heap_range = Some(ret..ret);
                    }
                } else if let Some(heap_range) = self.heap_range.as_mut() {
                    // either growing or shrinking the heap,
                    // and we know the previous top
                    let old_top = heap_range.end;
                    heap_range.end = ret;

                    if heap_range.end > old_top {
                        // heap just grew - shrinking will be handled by
                        // userfaultfd
                        return Ok(Some(Mapped {
                            range: old_top..heap_range.end,
                            resident: MemState::Resident,
                        }));
                    }
                }
            }
            _ => {
                // let's ignore those
            }
        }

        Ok(None)
    }
}
