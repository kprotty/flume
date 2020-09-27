// Copyright (c) 2020 kprotty
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

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