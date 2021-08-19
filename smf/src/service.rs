use std::ptr::NonNull;

use super::libscf::*;
use super::{Result, Scf, ScfError, Iter, Scope, Instances, buf_for, str_from};

#[derive(Debug)]
pub struct Service<'a> {
    pub(crate) scf: &'a Scf,
    pub(crate) service: NonNull<scf_service_t>,
}

impl<'a> Service<'a> {
    pub(crate) fn new(scf: &'a Scf) -> Result<Service> {
        if let Some(service) =
            NonNull::new(unsafe { scf_service_create(scf.handle.as_ptr()) })
        {
            Ok(Service { scf, service })
        } else {
            Err(ScfError::last())
        }
    }

    pub fn name(&self) -> Result<String> {
        let mut buf = buf_for(SCF_LIMIT_MAX_NAME_LENGTH)?;

        let ret = unsafe {
            scf_service_get_name(
                self.service.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };

        str_from(&mut buf, ret)
    }

    pub fn instances(&self) -> Result<Instances> {
        Instances::new(self)
    }

    /*
     * XXX fn delete(&self) -> Result<()> {
     * scf_service_delete(3SCF)
     */

    /*
     * XXX fn instance(&self, name: &str) -> Result<()> {
     * scf_service_get_instance(3SCF)
     */

    /*
     * XXX fn add(&self, name: &str) -> Result<()> {
     * scf_service_add_instance(3SCF)
     */
}

impl Drop for Service<'_> {
    fn drop(&mut self) {
        unsafe { scf_service_destroy(self.service.as_ptr()) };
    }
}

pub struct Services<'a> {
    scope: &'a Scope<'a>,
    iter: Iter<'a>,
}

impl<'a> Services<'a> {
    pub(crate) fn new(scope: &'a Scope) -> Result<Services<'a>> {
        let iter = Iter::new(scope.scf)?;

        if unsafe {
            scf_iter_scope_services(iter.iter.as_ptr(), scope.scope.as_ptr())
        } != 0
        {
            Err(ScfError::last())
        } else {
            Ok(Services { scope, iter })
        }
    }

    fn get(&self) -> Result<Option<Service<'a>>> {
        let service = Service::new(self.scope.scf)?;

        let res = unsafe {
            scf_iter_next_service(
                self.iter.iter.as_ptr(),
                service.service.as_ptr(),
            )
        };

        match res {
            0 => Ok(None),
            1 => Ok(Some(service)),
            _ => Err(ScfError::last()),
        }
    }
}

impl<'a> Iterator for Services<'a> {
    type Item = Result<Service<'a>>;

    fn next(&mut self) -> Option<Result<Service<'a>>> {
        self.get().transpose()
    }
}
