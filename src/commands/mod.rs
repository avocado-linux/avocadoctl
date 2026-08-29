pub mod ext;
pub mod hitl;
pub mod image_adaptor;
pub mod root_authority;
pub mod runtime;
pub mod var_key;

#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::Mutex;

    // Shared lock so any test that toggles process-global env vars
    // (AVOCADO_TEST_MODE, AVOCADO_TEST_TMPDIR, etc.) serializes against
    // every other such test, regardless of which module it lives in.
    pub(crate) static ENV_VAR_MUTEX: Mutex<()> = Mutex::new(());
}
