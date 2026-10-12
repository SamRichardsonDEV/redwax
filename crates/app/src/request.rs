use tokio::task::AbortHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

#[derive(Debug)]
struct Active<T> {
    request_id: RequestId,
    context: T,
    task_handle: Option<AbortHandle>,
    completed: bool,
}

/// Cancellation saves work. Identity also rejects completions already in the channel.
#[derive(Debug)]
pub struct RequestSlot<T> {
    request_generation: u64,
    active_request: Option<Active<T>>,
}

impl<T> Default for RequestSlot<T> {
    fn default() -> Self {
        Self {
            request_generation: 0,
            active_request: None,
        }
    }
}

impl<T> RequestSlot<T> {
    pub fn begin(&mut self, context: T) -> RequestId {
        self.cancel();
        self.request_generation = self
            .request_generation
            .checked_add(1)
            .expect("request generation cannot wrap during one application lifetime");
        let request_id = RequestId(self.request_generation);
        self.active_request = Some(Active {
            request_id,
            context,
            task_handle: None,
            completed: false,
        });
        request_id
    }

    pub fn hold(&mut self, request_id: RequestId, task_handle: AbortHandle) {
        match self
            .active_request
            .as_mut()
            .filter(|active_request| active_request.request_id == request_id)
        {
            Some(active_request) => {
                if let Some(previous_task_handle) = active_request.task_handle.replace(task_handle) {
                    previous_task_handle.abort();
                }
            }
            None => task_handle.abort(),
        }
    }

    pub fn get(&self, request_id: RequestId) -> Option<&T> {
        self.active_request
            .as_ref()
            .filter(|active_request| active_request.request_id == request_id)
            .map(|active_request| &active_request.context)
    }

    pub fn current(&self) -> Option<(RequestId, &T)> {
        self.active_request
            .as_ref()
            .map(|active_request| (active_request.request_id, &active_request.context))
    }

    pub fn complete(&mut self, request_id: RequestId) -> Option<&T> {
        let active_request = self
            .active_request
            .as_mut()
            .filter(|active_request| active_request.request_id == request_id && !active_request.completed)?;
        active_request.task_handle = None;
        active_request.completed = true;
        Some(&active_request.context)
    }

    pub fn cancel(&mut self) {
        if let Some(active_request) = self.active_request.take()
            && let Some(task_handle) = active_request.task_handle
        {
            task_handle.abort();
        }
    }
}

impl<T> Drop for RequestSlot<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_context_has_a_new_identity_and_rejects_the_old_result() {
        let mut slot = RequestSlot::default();
        let first_request = slot.begin("same query");
        let replacement_request = slot.begin("same query");
        assert_ne!(first_request, replacement_request);
        assert_eq!(slot.get(first_request), None);
        assert_eq!(slot.get(replacement_request), Some(&"same query"));
        slot.cancel();
        assert_eq!(slot.get(replacement_request), None);
        assert_ne!(slot.begin("same query"), replacement_request);
    }

    #[tokio::test]
    async fn replacing_a_request_cancels_its_owned_task() {
        let mut slot = RequestSlot::default();
        let first_request = slot.begin(());
        let request_task = tokio::spawn(std::future::pending::<()>());
        slot.hold(first_request, request_task.abort_handle());
        slot.begin(());
        assert!(
            request_task
                .await
                .expect_err("the replaced request is cancelled")
                .is_cancelled()
        );
    }
}
