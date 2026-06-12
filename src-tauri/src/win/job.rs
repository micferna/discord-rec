//! Job object « kill on close » : si l'app meurt sans arrêter proprement
//! l'enregistrement, Windows termine gst-launch à la fermeture du handle —
//! l'équivalent de `PR_SET_PDEATHSIG` sous Linux.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

pub struct JobHandle(HANDLE);

// SAFETY: le HANDLE de job object est utilisable depuis n'importe quel thread.
unsafe impl Send for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // SAFETY: handle valide créé par CreateJobObjectW.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn kill_on_close(pid: u32) -> Result<JobHandle> {
    // SAFETY: séquence Win32 documentée ; tous les handles sont refermés
    // (le handle de processus immédiatement, celui du job par Drop).
    unsafe {
        let job = CreateJobObjectW(None, None).context("création du job object")?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast(),
            u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("taille de structure Win32"),
        );
        if let Err(e) = set {
            let _ = CloseHandle(job);
            return Err(e).context("configuration kill-on-close du job");
        }
        let process = match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
            Ok(h) => h,
            Err(e) => {
                let _ = CloseHandle(job);
                return Err(e).context("ouverture du processus gst");
            }
        };
        let assign = AssignProcessToJobObject(job, process);
        let _ = CloseHandle(process);
        if let Err(e) = assign {
            let _ = CloseHandle(job);
            return Err(e).context("rattachement de gst au job");
        }
        Ok(JobHandle(job))
    }
}
