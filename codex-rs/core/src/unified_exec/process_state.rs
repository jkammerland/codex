#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProcessState {
    pub(crate) has_exited: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) failure_message: Option<String>,
    pub(crate) sandbox_denied: bool,
}

impl ProcessState {
    pub(crate) fn mark_exited(&mut self, exit_code: Option<i32>) {
        self.has_exited = true;
        self.exit_code = exit_code;
    }

    pub(crate) fn fail_if_absent(&mut self, message: String) {
        if self.failure_message.is_none() {
            self.has_exited = true;
            self.failure_message = Some(message);
        }
    }
}
