use std::sync::Mutex;

pub(crate) struct Lock<T>(Mutex<T>);

impl<T> Lock<T> {
    pub fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub fn with<F>(&self, f: impl FnOnce(&mut T) -> F) -> F {
        f(&mut *self.0.lock().unwrap())
    }
}