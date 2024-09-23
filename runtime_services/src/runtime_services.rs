//! Rust-friendly UEFI Runtime Service Wrappers
//!
//! Provides safe and unsafe easy-to-use wrappers for UEFI runtime services, as well as additional
//! utilities and helper functions.
//!
//! ```ignore
//! pub static RUNTIME_SERVICES: StandardRuntimeServices =
//!     StandardRuntimeServices::new(&(*runtime_services_ptr));
//! let variable_services::VariableInfo = RUNTIME_SERVICES.query_variable_info(attributes);
//! ```
//!

#![cfg_attr(all(not(test), not(feature = "mockall")), no_std)]

extern crate alloc;

/// Variable-services-specific structs and utilities
pub mod variable_services;

#[cfg(any(test, feature = "mockall"))]
use mockall::automock;

use alloc::vec::Vec;
use core::{
    ffi::c_void,
    marker::PhantomData,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use r_efi::efi;
use r_efi::efi::{Boolean, Time, TimeCapabilities};

use variable_services::{GetVariableStatus, VariableInfo};

/// The UEFI spec runtime services.
/// It wraps an [`AtomicPtr`] around [`efi::RuntimeServices`]
///
/// UEFI Spec Documentation: [8. Services - RuntimeServices](https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html)
///
#[derive(Debug)]
pub struct StandardRuntimeServices<'a> {
    efi_runtime_services: AtomicPtr<efi::RuntimeServices>,
    _lifetime_marker: PhantomData<&'a efi::RuntimeServices>,
}

impl<'a> StandardRuntimeServices<'a> {
    /// Create a new StandardRuntimeServices with the provided [efi::RuntimeServices].
    pub const fn new(efi_runtime_services: &'a efi::RuntimeServices) -> Self {
        // The efi::RuntimeServices is only read, that is why we use a non mutable reference.
        Self {
            efi_runtime_services: AtomicPtr::new(efi_runtime_services as *const _ as *mut _),
            _lifetime_marker: PhantomData,
        }
    }

    /// Create a new StandardRuntimeServices that is uninitialized.
    /// The struct need to be initialize later with [Self::initialize], otherwise, subsequent call will panic.
    pub const fn new_uninit() -> Self {
        Self { efi_runtime_services: AtomicPtr::new(ptr::null_mut()), _lifetime_marker: PhantomData }
    }

    /// Initialize the StandardRuntimeServices with a reference to [efi::RuntimeServices].
    /// # Debug asserts
    /// This function will assert on debug if already initialized.
    pub fn initialize(&'a self, efi_runtime_services: &'a efi::RuntimeServices) {
        if self.efi_runtime_services.load(Ordering::Relaxed).is_null() {
            // The efi::RuntimeServices is only read, that is why we use a non mutable reference.
            self.efi_runtime_services.store(efi_runtime_services as *const _ as *mut _, Ordering::SeqCst)
        } else {
            debug_assert!(false, "Runtime services is already initialized.");
        }
    }

    /// # Panics
    /// This function will panic if it was not initialize.
    fn efi_runtime_services(&self) -> &efi::RuntimeServices {
        // SAFETY: This pointer is assume to be a valid efi::RuntimeServices pointer since the only way to set it was via an efi::RuntimeServices reference.
        unsafe {
            self.efi_runtime_services
                .load(Ordering::SeqCst)
                .as_ref::<'a>()
                .expect("Runtime services is not initialized.")
        }
    }
}

///SAFETY: StandardRuntimeServices uses an atomic ptr to access the RuntimeServices.
unsafe impl Sync for StandardRuntimeServices<'static> {}
///SAFETY: When the lifetime is `'static`, the pointer is guaranteed to stay valid.
unsafe impl Send for StandardRuntimeServices<'static> {}

#[cfg_attr(any(test, feature = "mockall"), automock)]

/// Interface for Rust-friendly wrappers of the UEFI Runtime Services
pub trait RuntimeServices: Sized {
    /// Get the time.
    ///
    /// UEFI Spec Documentation: [8.2.3. EFI_RUNTIME_SERVICES.SetVariable()](https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#setvariable)
    ///
    fn set_variable<T>(
        &self,
        name: &[u16],
        namespace: &efi::Guid,
        attributes: u32,
        data: &T,
    ) -> Result<(), efi::Status>
    where
        T: AsRef<[u8]> + 'static,
    {
        if !name.iter().position(|&c| c == 0).is_some() {
            debug_assert!(false, "Name passed into set_variable is not null-terminated.");
            return Err(efi::Status::INVALID_PARAMETER);
        }

        // Keep a local copy of name to unburden the caller of having to pass in a mutable slice
        let mut name_vec = name.to_vec();

        unsafe { self.set_variable_unchecked(name_vec.as_mut_slice(), namespace, attributes, data.as_ref()) }
    }

    /// Gets a UEFI variable.
    ///
    /// Returns a tuple of (data, attributes)
    ///
    /// UEFI Spec Documentation: [8.2.1. EFI_RUNTIME_SERVICES.GetVariable()](https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#getvariable)
    ///
    fn get_variable<T>(
        &self,
        name: &[u16],
        namespace: &efi::Guid,
        size_hint: Option<usize>,
    ) -> Result<(T, u32), efi::Status>
    where
        T: TryFrom<Vec<u8>> + 'static,
    {
        if !name.iter().position(|&c| c == 0).is_some() {
            debug_assert!(false, "Name passed into get_variable is not null-terminated.");
            return Err(efi::Status::INVALID_PARAMETER);
        }

        // Keep a local copy of name to unburden the caller of having to pass in a mutable slice
        let mut name_vec = name.to_vec();

        // We can't simply allocate an empty buffer of size T because we can't assume
        // the TryFrom representation of T will be the same as T
        let mut data = Vec::<u8>::new();
        if size_hint.is_some() {
            data.resize(size_hint.unwrap(), 0);
        }

        // Do at most two calls to get_variable_unchecked.
        //
        // If size_hint was provided (and the size is sufficient), then only call to get_variable_unchecked is
        // needed. Otherwise, the first check will determine the size of the buffer to allocate for the second
        // call.
        let mut first_attempt = true;
        loop {
            unsafe {
                let status = self.get_variable_unchecked(
                    name_vec.as_mut_slice(),
                    namespace,
                    if data.len() == 0 { None } else { Some(&mut data) },
                );

                match status {
                    GetVariableStatus::Success { data_size: _, attributes } => match T::try_from(data) {
                        Ok(d) => return Ok((d, attributes)),
                        Err(_) => return Err(efi::Status::INVALID_PARAMETER),
                    },
                    GetVariableStatus::BufferTooSmall { data_size, attributes: _ } => {
                        if first_attempt {
                            first_attempt = false;
                            data.resize(data_size, 10);
                        } else {
                            return Err(efi::Status::BUFFER_TOO_SMALL);
                        }
                    }
                    GetVariableStatus::Error(e) => {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Helper function to get a UEFI variable's size and attributes
    fn get_variable_size_and_attributes(
        &self,
        name: &[u16],
        namespace: &efi::Guid,
    ) -> Result<(usize, u32), efi::Status> {
        if !name.iter().position(|&c| c == 0).is_some() {
            debug_assert!(false, "Name passed into set_variable is not null-terminated.");
            return Err(efi::Status::INVALID_PARAMETER);
        }

        // Keep a local copy of name to unburden the caller of having to pass in a mutable slice
        let mut name_vec = name.to_vec();

        unsafe {
            match self.get_variable_unchecked(name_vec.as_mut_slice(), namespace, None) {
                GetVariableStatus::BufferTooSmall { data_size, attributes } => Ok((data_size, attributes)),
                GetVariableStatus::Error(e) => Err(e),
                GetVariableStatus::Success { data_size, attributes } => {
                    debug_assert!(false, "GetVariable call with zero-sized buffer returned Success.");
                    Ok((data_size, attributes))
                }
            }
        }
    }

    /// Gets the name and namespace of the UEFI variable after the one provided.
    ///
    /// Returns a tuple of (name, namespace)
    ///
    /// Note: Unlike get_variable, a non-null terminated name will return INVALID_PARAMETER per UEFI spec
    ///
    /// UEFI Spec Documentation: [8.2.2. EFI_RUNTIME_SERVICES.GetNextVariableName()](https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#getnextvariablename)
    ///
    fn get_next_variable_name(
        &self,
        prev_name: &[u16],
        prev_namespace: &efi::Guid,
    ) -> Result<(Vec<u16>, efi::Guid), efi::Status> {
        if prev_name.len() == 0 {
            debug_assert!(false, "Zero-length name passed into get_next_variable_name.");
            return Err(efi::Status::INVALID_PARAMETER);
        }

        let mut next_name = Vec::<u16>::new();
        let mut next_namespace: efi::Guid = efi::Guid::from_bytes(&[0x0; 16]);

        unsafe {
            self.get_next_variable_name_unchecked(&prev_name, &prev_namespace, &mut next_name, &mut next_namespace)?;
        };

        Ok((next_name, next_namespace))
    }

    /// Queries variable information for given UEFI variable attributes.
    ///
    /// UEFI Spec Documentation: [8.2.4. EFI_RUNTIME_SERVICES.QueryVariableInfo()](https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#queryvariableinfo)
    ///
    fn query_variable_info(&self, attributes: u32) -> Result<VariableInfo, efi::Status>;

    /// UEFI Spec Documentation:
    /// <a href="https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#gettime" target="_blank">
    ///   8.3.1. GetTime()
    /// </a>
    ///
    /// [^note]: Time capabilities is always returned in this implementation.
    fn get_time(
        &self,
    ) -> Result<(Time, TimeCapabilities), efi::Status> {
        unsafe {
            self.get_time_unchecked()
        }
    } 

    /// Set the time.
    ///
    /// UEFI Spec Documentation:
    /// <a href="https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#settime" target="_blank">
    ///   8.3.2. SetTime()
    /// </a>
    fn set_time(
        &self,
        time: &efi::Time,
    ) -> Result<(), efi::Status> {
        unsafe {
            self.set_time_unchecked(time)
        }
    }

    /// Get the wake up time.
    ///
    /// UEFI Spec Documentation:
    /// <a href="https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#getwakeuptime" target="_blank">
    ///   8.3.3. GetWakeupTime()
    /// </a>
    fn get_wakeup_time(
        &self,
    ) -> Result<(bool, bool, Time), efi::Status> {
        unsafe {
            self.get_wakeup_time_unchecked()
        }
    }

    /// Set the wake up time.
    ///
    /// UEFI Spec Documentation:
    /// <a href="https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#setwakeuptime" target="_blank">
    ///   8.3.4. SetWakeupTime()
    /// </a>
    ///
    /// [^note]: Time must be present regardless of enable value in this implementation.
    fn set_wakeup_time(
        &self,
        enable: bool,
        time: &efi::Time,
    ) -> Result<(), efi::Status> {
        unsafe {
            self.set_wakeup_time_unchecked(enable, time)
        }
    }

    /// Prefer normal [`RuntimeServices::get_wakeup_time`] when possible.
    unsafe fn get_wakeup_time_unchecked(
        &self,
    ) -> Result<(bool, bool, Time), efi::Status>;

    /// Prefer normal [`RuntimeServices::set_time`] when possible.
    unsafe fn set_time_unchecked(
        &self,
        time: &efi::Time,
    ) -> Result<(), efi::Status>;

    /// Prefer normal [`RuntimeServices::get_time`] when possible.
    unsafe fn get_time_unchecked(
        &self,
    ) -> Result<(Time, TimeCapabilities), efi::Status>;

    /// Prefer normal [`RuntimeServices::set_wakeup_time`] when possible.
    unsafe fn set_wakeup_time_unchecked(
        &self,
        enable: bool,
        time: &efi::Time,
    ) -> Result<(), efi::Status>;

    /// Set's a UEFI variable
    ///
    /// # Safety
    ///
    /// Ensure name is null-terminated
    unsafe fn set_variable_unchecked(
        &self,
        name: &mut [u16],
        namespace: &efi::Guid,
        attributes: u32,
        data: &[u8],
    ) -> Result<(), efi::Status>;

    /// Gets a UEFI variable
    ///
    /// # Safety
    ///
    /// Ensure name is null-terminated
    unsafe fn get_variable_unchecked<'a>(
        &self,
        name: &mut [u16],
        namespace: &efi::Guid,
        data: Option<&'a mut [u8]>,
    ) -> GetVariableStatus;

    /// Gets the UEFI variable name after the one provided.
    ///
    /// Will populate next_name and next_namespace.
    ///
    /// # Safety
    ///
    /// Ensure name isn't empty. It can be an empty string,
    /// but there must be some data.
    ///
    unsafe fn get_next_variable_name_unchecked(
        &self,
        prev_name: &[u16],
        prev_namespace: &efi::Guid,
        next_name: &mut Vec<u16>,
        next_namespace: &mut efi::Guid,
    ) -> Result<(), efi::Status>;
}

impl RuntimeServices for StandardRuntimeServices<'_> {
    unsafe fn get_time_unchecked(&self) -> Result<(Time, TimeCapabilities), efi::Status> {
        let get_time = self.efi_runtime_services().get_time;
        if get_time as usize == 0 {
            panic!("function not initialize.")
        }
        let mut time: efi::Time = Default::default();
        let mut time_capabilities: efi::TimeCapabilities = efi::TimeCapabilities{resolution:0, accuracy:0, sets_to_zero: 0.into()};
        let status = get_time(&mut time as *mut efi::Time, &mut time_capabilities as *mut efi::TimeCapabilities);
        match status {
           efi::Status::SUCCESS => Ok((time, time_capabilities)),
           some_error => Err(some_error)
        }
    }

    unsafe fn set_time_unchecked(&self, time: &efi::Time) -> Result<(), efi::Status> {
        let set_time = self.efi_runtime_services().set_time;
        if set_time as usize == 0 {
            panic!("function not initialize.")
        }
        let status = set_time(time as *const efi::Time as *mut efi::Time);
        match status {
           efi::Status::SUCCESS => Ok(()),
           some_error => Err(some_error)
        }
    }

    unsafe fn get_wakeup_time_unchecked(&self) -> Result<(bool, bool, Time), efi::Status> {
        let get_wakeup_time = self.efi_runtime_services().get_wakeup_time;
        if get_wakeup_time as usize == 0 {
            panic!("function not initialize.")
        }
        let mut enabled: MaybeUninit::<Boolean> = MaybeUninit::zeroed();
        let mut pending: MaybeUninit::<Boolean> = MaybeUninit::zeroed();
        let mut time: MaybeUninit::<efi::Time> = MaybeUninit::zeroed();
        let status = get_wakeup_time(enabled.as_mut_ptr(), pending.as_mut_ptr(), time.as_mut_ptr());
        match status {
            efi::Status::SUCCESS => Ok((enabled.assume_init().into(), pending.assume_init().into(), time.assume_init())),
            some_error => Err(some_error)
        }
    }

    unsafe fn set_wakeup_time_unchecked(&self, enable: bool, time: &efi::Time) -> Result<(), efi::Status> {
        let set_wakeup_time = self.efi_runtime_services().set_wakeup_time;
        if set_wakeup_time as usize == 0 {
            panic!("function not initialize.")
        }
        let status = set_wakeup_time(enable.into(), time as *const efi::Time as *mut efi::Time);
        match status {
           efi::Status::SUCCESS => Ok(()),
           some_error => Err(some_error)
        }
    }

    unsafe fn set_variable_unchecked(
        &self,
        name: &mut [u16],
        namespace: &efi::Guid,
        attributes: u32,
        data: &[u8],
    ) -> Result<(), efi::Status> {
        let set_variable = self.efi_runtime_services().set_variable;
        if set_variable as usize == 0 {
            debug_assert!(false, "SetVariable has not initialized in the Runtime Services Table.");
            return Err(efi::Status::NOT_FOUND);
        }

        let status = set_variable(
            name.as_mut_ptr(),
            namespace as *const _ as *mut _,
            attributes,
            data.len(),
            data.as_ptr() as *mut c_void,
        );

        if status.is_error() {
            Err(status)
        } else {
            Ok(())
        }
    }

    unsafe fn get_variable_unchecked(
        &self,
        name: &mut [u16],
        namespace: &efi::Guid,
        data: Option<&mut [u8]>,
    ) -> GetVariableStatus {
        let get_variable = self.efi_runtime_services().get_variable;
        if get_variable as usize == 0 {
            debug_assert!(false, "GetVariable has not initialized in the Runtime Services Table.");
            return GetVariableStatus::Error(efi::Status::NOT_FOUND);
        }

        let mut data_size: usize = match data {
            Some(ref d) => d.len(),
            None => 0,
        };
        let mut attributes: u32 = 0;

        let status = get_variable(
            name.as_mut_ptr(),
            namespace as *const _ as *mut _,
            ptr::addr_of_mut!(attributes),
            ptr::addr_of_mut!(data_size),
            match data {
                Some(d) => d.as_ptr() as *mut c_void,
                None => ptr::null_mut() as *mut c_void,
            },
        );

        if status == efi::Status::BUFFER_TOO_SMALL {
            return GetVariableStatus::BufferTooSmall { data_size: data_size, attributes: attributes };
        } else if status.is_error() {
            return GetVariableStatus::Error(status);
        }

        GetVariableStatus::Success { data_size: data_size, attributes: attributes }
    }

    unsafe fn get_next_variable_name_unchecked(
        &self,
        prev_name: &[u16],
        prev_namespace: &efi::Guid,
        next_name: &mut Vec<u16>,
        next_namespace: &mut efi::Guid,
    ) -> Result<(), efi::Status> {
        let get_next_variable_name = self.efi_runtime_services().get_next_variable_name;
        if get_next_variable_name as usize == 0 {
            debug_assert!(false, "GetNextVariableName has not initialized in the Runtime Services Table.");
            return Err(efi::Status::NOT_FOUND);
        }

        // Copy prev_name and namespace into next name and namespace
        if next_name.len() < prev_name.len() {
            next_name.resize(prev_name.len(), 0);
        }
        next_name[..prev_name.len()].clone_from_slice(prev_name);
        next_namespace.clone_from(prev_namespace);

        let mut next_name_size: usize = next_name.len();

        // Loop at most two times. If the size of the previous name is sufficient for the next, then only
        // one call to the EFI function will be made. Otherwise, the first call will be used to determine
        // the appropriate size that the buffer should be resized to for the second call.
        let mut first_try: bool = true;
        loop {
            let status =
                get_next_variable_name(ptr::addr_of_mut!(next_name_size), next_name.as_mut_ptr(), next_namespace);

            if status == efi::Status::BUFFER_TOO_SMALL && first_try {
                first_try = false;

                assert!(
                    next_name_size > next_name.len(),
                    "get_next_variable_name requested smaller buffer on BUFFER_TOO_SMALL."
                );

                // Resize name to be able to fit the size of the next name
                next_name.resize(next_name_size, 0);

                // Reset fields which may have been overwritten
                next_name[..prev_name.len()].clone_from_slice(prev_name);
                next_namespace.clone_from(prev_namespace);
            } else if status.is_error() {
                return Err(status);
            } else {
                return Ok(());
            }
        }
    }

    fn query_variable_info(&self, attributes: u32) -> Result<VariableInfo, efi::Status> {
        let query_variable_info = self.efi_runtime_services().query_variable_info;
        if query_variable_info as usize == 0 {
            debug_assert!(false, "QueryVariableInfo has not initialized in the Runtime Services Table.");
            return Err(efi::Status::NOT_FOUND);
        }

        let mut var_info = VariableInfo {
            maximum_variable_storage_size: 0,
            remaining_variable_storage_size: 0,
            maximum_variable_size: 0,
        };

        let status = query_variable_info(
            attributes,
            ptr::addr_of_mut!(var_info.maximum_variable_storage_size),
            ptr::addr_of_mut!(var_info.remaining_variable_storage_size),
            ptr::addr_of_mut!(var_info.maximum_variable_size),
        );

        if status.is_error() {
            return Err(status);
        } else {
            return Ok(var_info);
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use efi;

    use super::*;
    use core::{mem, slice};

    macro_rules! runtime_services {
        ($($efi_services:ident = $efi_service_fn:ident),*) => {{
        static RUNTIME_SERVICE: StandardRuntimeServices = StandardRuntimeServices::new_uninit();
        let efi_runtime_services = unsafe {
            #[allow(unused_mut)]
            let mut rs = mem::MaybeUninit::<efi::RuntimeServices>::zeroed();
            $(
            rs.assume_init_mut().$efi_services = $efi_service_fn;
            )*
            rs.assume_init()
        };
        RUNTIME_SERVICE.initialize(&efi_runtime_services);
        &RUNTIME_SERVICE
        }};
    }

    pub(crate) use runtime_services;

    #[test]
    #[should_panic(expected = "Runtime services is not initialized.")]
    fn test_that_accessing_uninit_runtime_services_should_panic() {
        let rs = StandardRuntimeServices::new_uninit();
        rs.efi_runtime_services();
    }

    #[test]
    #[should_panic(expected = "Runtime services is already initialized.")]
    fn test_that_initializing_runtime_services_multiple_time_should_panic() {
        let efi_rs = unsafe { mem::MaybeUninit::<efi::RuntimeServices>::zeroed().as_ptr().as_ref().unwrap() };
        let rs = StandardRuntimeServices::new_uninit();
        rs.initialize(efi_rs);
        rs.initialize(efi_rs);
    }

    pub const DUMMY_FIRST_NAME: [u16; 3] = [0x1000, 0x1020, 0x0000];
    pub const DUMMY_NON_NULL_TERMINATED_NAME: [u16; 3] = [0x1000, 0x1020, 0x1040];
    pub const DUMMY_EMPTY_NAME: [u16; 1] = [0x0000];
    pub const DUMMY_ZERO_LENGTH_NAME: [u16; 0] = [];
    pub const DUMMY_SECOND_NAME: [u16; 5] = [0x1001, 0x1022, 0x1043, 0x1064, 0x0000];
    pub const DUMMY_UNKNOWN_NAME: [u16; 3] = [0x2000, 0x2020, 0x0000];

    pub const DUMMY_NODE: [u8; 6] = [0x0, 0x0, 0x0, 0x0, 0x0, 0x0];
    pub const DUMMY_FIRST_NAMESPACE: efi::Guid = efi::Guid::from_fields(0, 0, 0, 0, 0, &DUMMY_NODE);
    pub const DUMMY_SECOND_NAMESPACE: efi::Guid = efi::Guid::from_fields(1, 0, 0, 0, 0, &DUMMY_NODE);

    pub const DUMMY_ATTRIBUTES: u32 = 0x1234;
    pub const DUMMY_INVALID_ATTRIBUTES: u32 = 0x2345;

    pub const DUMMY_DATA: u32 = 0xDEADBEEF;
    pub const DUMMY_DATA_REPR_SIZE: usize = mem::size_of::<u32>();

    pub const DUMMY_MAXIMUM_VARIABLE_STORAGE_SIZE: u64 = 0x11111111_11111111;
    pub const DUMMY_REMAINING_VARIABLE_STORAGE_SIZE: u64 = 0x22222222_22222222;
    pub const DUMMY_MAXIMUM_VARIABLE_SIZE: u64 = 0x33333333_33333333;

    #[derive(Debug)]
    pub struct DummyVariableType {
        pub value: u32,
    }

    impl AsRef<[u8]> for DummyVariableType {
        fn as_ref(&self) -> &[u8] {
            unsafe { slice::from_raw_parts::<u8>(ptr::addr_of!(self.value) as *mut u8, mem::size_of::<u32>()) }
        }
    }

    impl TryFrom<Vec<u8>> for DummyVariableType {
        type Error = &'static str;

        fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
            assert!(value.len() == mem::size_of::<u32>());

            Ok(DummyVariableType { value: u32::from_ne_bytes(value[0..4].try_into().unwrap()) })
        }
    }

    /// Mocks GetVariable() from UEFI spec
    ///
    /// Expects to be passed DUMMY_FIRST_NAME, DUMMY_FIRST_NAMESPACE, and to return
    /// DUMMY_ATTRIBUTES, and DUMMY_DATA.
    ///
    /// DUMMY_UNKNOWN_NAME can be passed in to test searching for non-existant variables.
    ///
    pub extern "efiapi" fn mock_efi_get_variable(
        name: *mut u16,
        namespace: *mut efi::Guid,
        attributes: *mut u32,
        data_size: *mut usize,
        data: *mut c_void,
    ) -> efi::Status {
        unsafe {
            if DUMMY_UNKNOWN_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c) {
                return efi::Status::NOT_FOUND;
            }

            // Since name isn't DUMMY_UNKNOWN_NAME, we're assuming DUMMY_FIRST_NAME was passed in.
            // If name is not equal to DUMMY_FIRST_NAME, then something must have gone wrong.
            assert_eq!(
                DUMMY_FIRST_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c),
                true,
                "Variable name does not match expected."
            );

            assert_eq!(*namespace, DUMMY_FIRST_NAMESPACE);

            *attributes = DUMMY_ATTRIBUTES;

            if *data_size < DUMMY_DATA_REPR_SIZE {
                *data_size = DUMMY_DATA_REPR_SIZE;
                return efi::Status::BUFFER_TOO_SMALL;
            }

            *data_size = DUMMY_DATA_REPR_SIZE;
            *(data as *mut u32) = DUMMY_DATA;
        }

        efi::Status::SUCCESS
    }

    /// Mocks SetVariable() from UEFI spec
    ///
    /// Expects to be passed DUMMY_FIRST_NAME, DUMMY_FIRST_NAMESPACE, and DUMMY_DATA
    ///
    /// DUMMY_UNKNOWN_NAME can be passed in to test searching for non-existant variables.
    ///
    pub extern "efiapi" fn mock_efi_set_variable(
        name: *mut u16,
        namespace: *mut efi::Guid,
        attributes: u32,
        data_size: usize,
        data: *mut c_void,
    ) -> efi::Status {
        unsafe {
            // Invalid parameter is returned if name is empty (first character is 0)
            if *name == 0 {
                return efi::Status::INVALID_PARAMETER;
            }

            if DUMMY_UNKNOWN_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c) {
                return efi::Status::NOT_FOUND;
            }

            // Since name isn't DUMMY_UNKNOWN_NAME, we're assuming DUMMY_FIRST_NAME was passed in.
            // If name is not equal to DUMMY_FIRST_NAME, then something must have gone wrong.
            assert_eq!(
                DUMMY_FIRST_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c),
                true,
                "Variable name does not match expected."
            );

            assert_eq!(*namespace, DUMMY_FIRST_NAMESPACE);
            assert_eq!(attributes, DUMMY_ATTRIBUTES);
            assert_eq!(data_size, DUMMY_DATA_REPR_SIZE);
            assert_eq!(*(data as *mut u32), DUMMY_DATA);
        }

        efi::Status::SUCCESS
    }

    /// Mocks GetNextVariableName() from UEFI spec
    ///
    /// Will mock a list of two variables:
    ///     1. DUMMY_FIRST_NAME (under namespace DUMMY_FIRST_NAMESPACE)
    ///     2. DUMMY_SECOND_NAME (under namespace DUMMY_SECOND_NAME)
    ///
    /// DUMMY_UNKNOWN_NAME can be passed in to test searching for non-existant variables.
    ///
    pub extern "efiapi" fn mock_efi_get_next_variable_name(
        name_size: *mut usize,
        name: *mut u16,
        namespace: *mut efi::Guid,
    ) -> efi::Status {
        // Ensure the name and namespace are as expected
        unsafe {
            // Return invalid parameter if the name isn't null-terminated per UEFI spec
            if !slice::from_raw_parts(name, *name_size).iter().position(|&c| c == 0).is_some() {
                return efi::Status::INVALID_PARAMETER;
            }

            if DUMMY_UNKNOWN_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c) {
                return efi::Status::NOT_FOUND;
            }

            // If name is an empty string, return the first variable
            if *name == 0 {
                if *name_size < DUMMY_FIRST_NAME.len() {
                    *name_size = DUMMY_FIRST_NAME.len();
                    return efi::Status::BUFFER_TOO_SMALL;
                }

                *name_size = DUMMY_FIRST_NAME.len();
                ptr::copy_nonoverlapping(DUMMY_FIRST_NAME.as_ptr(), name, DUMMY_FIRST_NAME.len());
                *namespace = DUMMY_FIRST_NAMESPACE;

                return efi::Status::SUCCESS;
            }

            // If the first variable is passed in, return the second
            if DUMMY_FIRST_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c) {
                assert_eq!(*namespace, DUMMY_FIRST_NAMESPACE);

                if *name_size < DUMMY_SECOND_NAME.len() {
                    *name_size = DUMMY_SECOND_NAME.len();
                    return efi::Status::BUFFER_TOO_SMALL;
                }

                *name_size = DUMMY_SECOND_NAME.len();
                ptr::copy_nonoverlapping(DUMMY_SECOND_NAME.as_ptr(), name, DUMMY_SECOND_NAME.len());
                *namespace = DUMMY_SECOND_NAMESPACE;

                return efi::Status::SUCCESS;
            }

            // If the second (and last) variable is passed in, return NOT_FOUND to indicate the end of the list per
            // UEFI spec
            if DUMMY_SECOND_NAME.iter().enumerate().all(|(i, &c)| *name.offset(i as isize) == c) {
                assert_eq!(*namespace, DUMMY_SECOND_NAMESPACE);
                return efi::Status::NOT_FOUND;
            }

            // If we got here, the variable name must have gotten lost or corrupted somehow
            assert!(false, "Variable name does not match any of expected.");
        }

        efi::Status::SUCCESS
    }

    /// Mocks QueryVariableInfo() from UEFI spec
    ///
    /// Expects to be passed DUMMY_ATTRIBUTES, and to return, DUMMY_MAXIMUM_VARIABLE_STORAGE_SIZE,
    /// DUMMY_REMAINING_VARIABLE_STORAGE_SIZE, and DUMMY_MAXIMUM_VARIABLE_SIZE.
    ///
    /// DUMMY_INVALID_ATTRIBUTES can be passed in to test querying invalid attributes.
    ///
    pub extern "efiapi" fn mock_efi_query_variable_info(
        attributes: u32,
        maximum_variable_storage_size: *mut u64,
        remaining_variable_storage_size: *mut u64,
        maximum_variable_size: *mut u64,
    ) -> efi::Status {
        if attributes == DUMMY_INVALID_ATTRIBUTES {
            return efi::Status::INVALID_PARAMETER;
        }

        // Since attributes isn't DUMMY_INVALID_ATTRIBUTES, we're assuming DUMMY_ATTRIBUTES was passed in.
        // If attributes is not equal to DUMMY_ATTRIBUTES, then something must have gone wrong.
        assert_eq!(attributes, DUMMY_ATTRIBUTES);

        unsafe {
            *maximum_variable_storage_size = DUMMY_MAXIMUM_VARIABLE_STORAGE_SIZE;
            *remaining_variable_storage_size = DUMMY_REMAINING_VARIABLE_STORAGE_SIZE;
            *maximum_variable_size = DUMMY_MAXIMUM_VARIABLE_SIZE;
        }

        efi::Status::SUCCESS
    }

    #[test]
    fn test_get_variable() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(get_variable = mock_efi_get_variable);

        let status = rs.get_variable::<DummyVariableType>(&DUMMY_FIRST_NAME, &DUMMY_FIRST_NAMESPACE, None);

        assert!(status.is_ok());
        let (data, attributes) = status.unwrap();
        assert_eq!(attributes, DUMMY_ATTRIBUTES);
        assert_eq!(data.value, DUMMY_DATA);
    }

    #[test]
    #[should_panic(expected = "Name passed into get_variable is not null-terminated.")]
    fn test_get_variable_non_terminated() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(get_variable = mock_efi_get_variable);

        let _ = rs.get_variable::<DummyVariableType>(&DUMMY_NON_NULL_TERMINATED_NAME, &DUMMY_FIRST_NAMESPACE, None);
    }

    #[test]
    fn test_get_variable_low_size_hint() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(get_variable = mock_efi_get_variable);

        let status = rs.get_variable::<DummyVariableType>(&DUMMY_FIRST_NAME, &DUMMY_FIRST_NAMESPACE, Some(1));

        assert!(status.is_ok());
        let (data, attributes) = status.unwrap();
        assert_eq!(attributes, DUMMY_ATTRIBUTES);
        assert_eq!(data.value, DUMMY_DATA);
    }

    #[test]
    fn test_get_variable_not_found() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(get_variable = mock_efi_get_variable);

        let status = rs.get_variable::<DummyVariableType>(&DUMMY_UNKNOWN_NAME, &DUMMY_FIRST_NAMESPACE, Some(1));

        assert!(status.is_err());
        assert_eq!(status.unwrap_err(), efi::Status::NOT_FOUND);
    }

    #[test]
    fn test_get_variable_size_and_attributes() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(get_variable = mock_efi_get_variable);

        let status = rs.get_variable_size_and_attributes(&DUMMY_FIRST_NAME, &DUMMY_FIRST_NAMESPACE);

        assert!(status.is_ok());
        let (size, attributes) = status.unwrap();
        assert_eq!(size, DUMMY_DATA_REPR_SIZE);
        assert_eq!(attributes, DUMMY_ATTRIBUTES);
    }

    #[test]
    fn test_set_variable() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(set_variable = mock_efi_set_variable);

        let mut data = DummyVariableType { value: DUMMY_DATA };

        let status = rs.set_variable::<DummyVariableType>(
            &DUMMY_FIRST_NAME,
            &DUMMY_FIRST_NAMESPACE,
            DUMMY_ATTRIBUTES,
            &mut data,
        );

        assert!(status.is_ok());
    }

    #[test]
    #[should_panic(expected = "Name passed into set_variable is not null-terminated.")]
    fn test_set_variable_non_terminated() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(set_variable = mock_efi_set_variable);

        let mut data = DummyVariableType { value: DUMMY_DATA };

        let _ = rs.set_variable::<DummyVariableType>(
            &DUMMY_NON_NULL_TERMINATED_NAME,
            &DUMMY_FIRST_NAMESPACE,
            DUMMY_ATTRIBUTES,
            &mut data,
        );
    }

    #[test]
    fn test_set_variable_empty_name() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(set_variable = mock_efi_set_variable);

        let mut data = DummyVariableType { value: DUMMY_DATA };

        let status = rs.set_variable::<DummyVariableType>(
            &DUMMY_EMPTY_NAME,
            &DUMMY_FIRST_NAMESPACE,
            DUMMY_ATTRIBUTES,
            &mut data,
        );

        assert!(status.is_err());
        assert_eq!(status.unwrap_err(), efi::Status::INVALID_PARAMETER);
    }

    #[test]
    fn test_set_variable_not_found() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(set_variable = mock_efi_set_variable);

        let mut data = DummyVariableType { value: DUMMY_DATA };

        let status = rs.set_variable::<DummyVariableType>(
            &DUMMY_UNKNOWN_NAME,
            &DUMMY_FIRST_NAMESPACE,
            DUMMY_ATTRIBUTES,
            &mut data,
        );

        assert!(status.is_err());
        assert_eq!(status.unwrap_err(), efi::Status::NOT_FOUND);
    }

    #[test]
    fn test_get_next_variable_name() {
        // Ensure we are testing a growing name buffer
        assert!(DUMMY_SECOND_NAME.len() > DUMMY_FIRST_NAME.len());

        let rs: &StandardRuntimeServices<'_> =
            runtime_services!(get_next_variable_name = mock_efi_get_next_variable_name);

        let status = rs.get_next_variable_name(&DUMMY_FIRST_NAME, &DUMMY_FIRST_NAMESPACE);

        assert!(status.is_ok());

        let (next_name, next_guid) = status.unwrap();

        assert_eq!(next_name, DUMMY_SECOND_NAME);
        assert_eq!(next_guid, DUMMY_SECOND_NAMESPACE);
    }

    #[test]
    fn test_get_next_variable_name_non_terminated() {
        let rs: &StandardRuntimeServices<'_> =
            runtime_services!(get_next_variable_name = mock_efi_get_next_variable_name);

        let status = rs.get_next_variable_name(&DUMMY_NON_NULL_TERMINATED_NAME, &DUMMY_FIRST_NAMESPACE);

        assert!(status.is_err());
        assert_eq!(status.unwrap_err(), efi::Status::INVALID_PARAMETER);
    }

    #[test]
    #[should_panic(expected = "Zero-length name passed into get_next_variable_name.")]
    fn test_get_next_variable_name_zero_length_name() {
        let rs: &StandardRuntimeServices<'_> =
            runtime_services!(get_next_variable_name = mock_efi_get_next_variable_name);

        let _ = rs.get_next_variable_name(&DUMMY_ZERO_LENGTH_NAME, &DUMMY_FIRST_NAMESPACE);
    }

    #[test]
    fn test_get_next_variable_name_not_found() {
        let rs: &StandardRuntimeServices<'_> =
            runtime_services!(get_next_variable_name = mock_efi_get_next_variable_name);

        let status = rs.get_next_variable_name(&DUMMY_UNKNOWN_NAME, &DUMMY_FIRST_NAMESPACE);

        assert!(status.is_err());
        assert_eq!(status.unwrap_err(), efi::Status::NOT_FOUND);
    }

    #[test]
    fn test_query_variable_info() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(query_variable_info = mock_efi_query_variable_info);

        let status = rs.query_variable_info(DUMMY_ATTRIBUTES);

        assert!(status.is_ok());
        let variable_info = status.unwrap();
        assert_eq!(variable_info.maximum_variable_storage_size, DUMMY_MAXIMUM_VARIABLE_STORAGE_SIZE);
        assert_eq!(variable_info.remaining_variable_storage_size, DUMMY_REMAINING_VARIABLE_STORAGE_SIZE);
        assert_eq!(variable_info.maximum_variable_size, DUMMY_MAXIMUM_VARIABLE_SIZE);
    }

    #[test]
    fn test_query_variable_info_invalid_attributes() {
        let rs: &StandardRuntimeServices<'_> = runtime_services!(query_variable_info = mock_efi_query_variable_info);

        let status = rs.query_variable_info(DUMMY_INVALID_ATTRIBUTES);

        assert!(status.is_err());
        assert_eq!(status.unwrap_err(), efi::Status::INVALID_PARAMETER);
    }
}
