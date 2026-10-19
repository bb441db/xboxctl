use core_foundation::{
    base::{CFType, TCFType, kCFAllocatorDefault},
    number::CFNumber,
    string::CFString,
};
use io_kit_sys::{
    IOIteratorNext, IOObjectGetClass, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntryGetChildIterator, IOServiceGetMatchingServices, IOServiceMatching,
    keys::kIOServicePlane,
    types::{io_iterator_t, io_object_t},
};
use std::ffi::{CStr, c_char};

/// An entry in the IOKit registry, released when dropped.
pub struct Entry(io_object_t);

impl Drop for Entry {
    fn drop(&mut self) {
        // SAFETY: the object came from an IOKit call that handed us a
        // reference, and it is released exactly once, here.
        unsafe { IOObjectRelease(self.0) };
    }
}

impl Entry {
    /// Every object an iterator yields. Consumes and releases the iterator.
    fn drain(iterator: io_iterator_t) -> Vec<Entry> {
        let iterator = Entry(iterator);
        let mut entries = Vec::new();
        loop {
            // SAFETY: `iterator` is a valid iterator until it is dropped.
            let next = unsafe { IOIteratorNext(iterator.0) };
            if next == 0 {
                return entries;
            }
            entries.push(Entry(next));
        }
    }

    /// All registered services of the IOKit class `class`.
    pub fn services_of_class(class: &CStr) -> Vec<Entry> {
        // SAFETY: `class` is a valid C string; IOServiceMatching copies it.
        let matching = unsafe { IOServiceMatching(class.as_ptr()) };
        let mut iterator = 0;
        // SAFETY: a valid dictionary, which the call consumes, and a valid out
        // pointer. Port 0 is the default main port.
        let status = unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator) };
        if status == 0 {
            Self::drain(iterator)
        } else {
            Vec::new()
        }
    }

    /// The entry's children in the service plane.
    pub fn children(&self) -> Vec<Entry> {
        let mut iterator = 0;
        // SAFETY: `self` is valid and `iterator` is a valid out pointer.
        let status =
            unsafe { IORegistryEntryGetChildIterator(self.0, kIOServicePlane, &mut iterator) };
        if status == 0 {
            Self::drain(iterator)
        } else {
            Vec::new()
        }
    }

    /// The IOKit class name, e.g. `AppleUSBHostDeviceUserClient`.
    pub fn class_name(&self) -> String {
        let mut name = [0 as c_char; 128];
        // SAFETY: IOKit class names fit the 128 bytes it documents.
        let status = unsafe { IOObjectGetClass(self.0, name.as_mut_ptr()) };
        if status != 0 {
            return String::new();
        }
        // SAFETY: on success the buffer holds a NUL-terminated string.
        unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    fn property(&self, key: &str) -> Option<CFType> {
        let key = CFString::new(key);
        // SAFETY: both arguments are valid; a non-null result is a +1
        // reference, which `wrap_under_create_rule` takes over.
        unsafe {
            let value = IORegistryEntryCreateCFProperty(
                self.0,
                key.as_concrete_TypeRef(),
                kCFAllocatorDefault,
                0,
            );
            (!value.is_null()).then(|| CFType::wrap_under_create_rule(value))
        }
    }

    /// A string property, if the entry has one.
    pub fn string(&self, key: &str) -> Option<String> {
        Some(self.property(key)?.downcast::<CFString>()?.to_string())
    }

    /// An integer property, if the entry has one.
    pub fn number(&self, key: &str) -> Option<i64> {
        self.property(key)?.downcast::<CFNumber>()?.to_i64()
    }
}
