use super::*;
pub(super) type Hook = Box<dyn FnMut(&Runtime)>;
struct Boundary<'a>(&'a Cell<usize>, usize);
impl Drop for Boundary<'_> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}
impl Runtime {
    pub(super) fn boundary<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R> {
        let mut f = Protected(self, Some(f));
        let old = self.0.notification_depth.get();
        self.0
            .notification_depth
            .set(old.checked_add(1).ok_or_else(|| self.exhausted())?);
        let boundary = Boundary(&self.0.notification_depth, old);
        let mut result = Protected(self, Some(self.user(|| f.1.take().unwrap()())));
        drop(boundary);
        // Local errors (notably UpdateConflict) do not roll back nested writes.
        // Notification belongs to the outer operation, including its Err exit.
        self.notify();
        result.1.take().unwrap()
    }
    pub fn set_request_flush_hook(
        &self,
        hook: Option<impl FnMut(&Runtime) + 'static>,
    ) -> Result<()> {
        let mut hook = Protected(self, Some(hook.map(|h| Box::new(h) as Hook)));
        self.check(CREATE)?;
        let old = self.0.hook.replace(hook.1.take().unwrap());
        self.0.notification_outstanding.set(false);
        self.retire(old);
        self.notify();
        Ok(())
    }
    pub fn clear_request_flush_hook(&self) -> Result<()> {
        if CAPS.get() & CREATE == 0 {
            return Err(capability_error(CREATE));
        }
        let old = self.0.hook.take();
        self.0.notification_outstanding.set(false);
        self.retire(old);
        Ok(())
    }
    pub fn rearm_flush_notification(&self) -> Result<()> {
        self.check(CREATE)?;
        self.0.notification_outstanding.set(false);
        self.notify();
        Ok(())
    }
    pub fn flush_notification_outstanding(&self) -> bool {
        self.0.notification_outstanding.get()
    }
    pub(super) fn notify(&self) {
        if CAPS.get() & FLUSH == 0
            || self.0.status.get() != 0
            || self.0.notification_depth.get() != 0
            || self.0.batch.get() != 0
            || self.0.depth.get() != 0
            || self.0.disposal.get() != 0
            || self.0.flushing.get()
            || self.0.notification_outstanding.get()
            || !self.has_pending_effects()
        {
            return;
        }
        let Some(mut hook) = self.0.hook.borrow_mut().take() else {
            return;
        };
        self.0.notification_outstanding.set(true);
        let _caps = Caps::enter(0);
        let _reader = ReaderRuntime::enter(None);
        let _callback = shared_plan::Callback::enter(self);
        let outcome = catch_unwind(AssertUnwindSafe(|| hook(self)));
        *self.0.hook.borrow_mut() = Some(hook);
        if let Err(p) = outcome {
            self.record_failure(Error::Panic);
            resume_unwind(p);
        }
    }
}
