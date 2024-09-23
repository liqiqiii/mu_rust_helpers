#![cfg_attr(all(not(test), not(feature = "mockall")), no_std)]

#[cfg(feature = "global_allocator")]
pub mod global_allocator;

extern crate alloc;

pub mod allocation;
pub mod boxed;
pub mod static_ptr;

#[cfg(any(test, feature = "mockall"))]
use mockall::automock;

use alloc::vec::Vec;
use core::{
    any::{Any, TypeId},
    ffi::c_void,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    option::Option,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};
use static_ptr::{StaticPtr, StaticPtrMut};

use r_efi::efi;

use allocation::{AllocType, MemoryMap, MemoryType};
use boxed::RuntimeServicesBox;

// #[derive(Debug, Clone, Copy)]
// #[repr(u32)]
// pub enum ResetSystemType {
//     ///
//     /// Used to induce a system-wide reset. This sets all circuitry within the
//     /// system to its initial state.  This type of reset is asynchronous to system
//     /// operation and operates withgout regard to cycle boundaries.  EfiColdReset
//     /// is tantamount to a system power cycle.
//     ///
//     EfiResetCold = efi::RESET_COLD,
//     ///
//     /// Used to induce a system-wide initialization. The processors are set to their
//     /// initial state, and pending cycles are not corrupted.  If the system does
//     /// not support this reset type, then an EfiResetCold must be performed.
//     ///
//     EfiResetWarm = efi::RESET_WARM,
//     ///
//     /// Used to induce an entry into a power state equivalent to the ACPI G2/S5 or G3
//     /// state.  If the system does not support this reset type, then when the system
//     /// is rebooted, it should exhibit the EfiResetCold attributes.
//     ///
//     EfiResetShutdown = efi::RESET_SHUTDOWN,
//     ///
//     /// Used to induce a system-wide reset. The exact type of the reset is defined by
//     /// the EFI_GUID that follows the Null-terminated Unicode string passed into
//     /// ResetData. If the platform does not recognize the EFI_GUID in ResetData the
//     /// platform must pick a supported reset type to perform. The platform may
//     /// optionally log the parameters from any non-normal reset that occurs.
//     ///
//     EfiResetPlatformSpecific= efi::RESET_PLATFORM_SPECIFIC,
// }

/// This is the runtime services used in the UEFI.
/// it wraps an atomic ptr to [`efi::RuntimeServices`]
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
    /// # Panics
    /// This function will panic if already initialize.
    pub fn initialize(&'a self, efi_runtime_services: &'a efi::RuntimeServices) {
        if self.efi_runtime_services.load(Ordering::Relaxed).is_null() {
            // The efi::RuntimeServices is only read, that is why we use a non mutable reference.
            self.efi_runtime_services.store(efi_runtime_services as *const _ as *mut _, Ordering::SeqCst)
        } else {
            panic!("Runtime services is already initialize.")
        }
    }

    /// # Panics
    /// This function will panic if it was not initialize.
    fn efi_runtime_services(&self) -> &efi::RuntimeServices {
        // SAFETY: This pointer is assume to be a valid efi::RuntimeServices pointer since the only way to set it was via an efi::RuntimeServices reference.
        unsafe {
            self.efi_runtime_services.load(Ordering::SeqCst).as_ref::<'a>().expect("Runtime services is not initialize.")
        }
    }
}

///SAFETY: StandardRuntimeServices uses an atomic ptr to access the RuntimeServices.
unsafe impl Sync for StandardRuntimeServices<'static> {}
///SAFETY: When the lifetime is `'static`, the pointer is guaranteed to stay valid.
unsafe impl Send for StandardRuntimeServices<'static> {}

#[cfg_attr(any(test, feature = "mockall"), automock)]
pub trait RuntimeServices: Sized {
    /// Create an event.
    ///
    /// UEFI Spec Documentation:
    /// <a href="https://uefi.org/specs/UEFI/2.10/08_Services_Runtime_Services.html#reset-system" target="_blank">
    ///   7.1.1. EFI_RUNTIME_SERVICES.ResetSystem()
    /// </a>
    fn reset_system (
      self,
      reset_type: u32,
      reset_status: efi::Status,
      data_size: usize,
      reset_data: *mut c_void,
    );
}

impl RuntimeServices for StandardRuntimeServices<'_> {

  fn reset_system (
    self,
    reset_type: u32,
    reset_status: efi::Status,
    data_size: usize,
    reset_data: *mut c_void,
  ) {
    let reset_system = self.efi_runtime_services().reset_system;
    if reset_system as usize == 0 {
      panic!("function not initialize.")
    }
    reset_system(reset_type, reset_status, data_size, reset_data);
  }

}

#[cfg(test)]
mod test {
    use efi;

    use super::*;
    use core::{mem::MaybeUninit, sync::atomic::AtomicUsize};

    macro_rules! runtime_services {
    ($($efi_services:ident = $efi_service_fn:ident),*) => {{
      static RUNTIME_SERVICE: StandardRuntimeServices = StandardRuntimeServices::new_uninit();
      let efi_runtime_services = unsafe {
        #[allow(unused_mut)]
        let mut bs = MaybeUninit::<efi::RuntimeServices>::zeroed();
        $(
          bs.assume_init_mut().$efi_services = $efi_service_fn;
        )*
        bs.assume_init()
      };
      RUNTIME_SERVICE.initialize(&efi_runtime_services);
      &RUNTIME_SERVICE
    }};
  }

    #[test]
    #[should_panic(expected = "Runtime services is not initialized.")]
    fn test_that_accessing_uninit_runtime_services_should_panic() {
        let bs = StandardRuntimeServices::new_uninit();
        bs.efi_runtime_services();
    }

    #[test]
    #[should_panic(expected = "Runtime services is already initialized.")]
    fn test_that_initializing_runtime_services_multiple_time_should_panic() {
        let efi_bs = unsafe { MaybeUninit::<efi::RuntimeServices>::zeroed().as_ptr().as_ref().unwrap() };
        let bs = StandardRuntimeServices::new_uninit();
        bs.initialize(efi_bs);
        bs.initialize(efi_bs);
    }

    #[test]
    #[should_panic = "Run time services is not initialize."]
    fn test_reset_work() {


      let runtime_services = runtime_services!(reset_system = efi_reset_system);

      extern "efiapi" fn efi_reset_system(
        reset_type: u32,
        reset_status: efi::Status,
        data_size: usize,
        reset_data: *mut c_void,){}
      runtime_services.reset_system(efi::RESET_COLD, efi::Status::SUCCESS, 5, ptr::null_mut());
    }

}