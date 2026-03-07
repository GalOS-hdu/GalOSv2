use core::ffi::{c_char, c_int};
extern crate alloc;

use axerrno::{AxError, AxResult, LinuxError};

/// 将 xattr 属性不存在的 ENOENT 映射为 POSIX 规定的 ENODATA
fn noent_to_nodata(e: AxError) -> AxError {
    if LinuxError::from(e) == LinuxError::ENOENT {
        AxError::from(LinuxError::ENODATA)
    } else {
        e
    }
}
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use starry_vm::{VmPtr, vm_read_slice, vm_write_slice};

use crate::{
    file::{Directory, File, get_file_like, with_fs},
    mm::vm_load_string,
};

pub fn sys_getxattr(
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    sys_fgetxattrat(AT_FDCWD, path, name, value, size, 0)
}

pub fn sys_lgetxattr(
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    sys_fgetxattrat(AT_FDCWD, path, name, value, size, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fgetxattr(
    fd: c_int,
    name: *const c_char,
    value: *mut u8,
    size: usize,
) -> AxResult<isize> {
    sys_fgetxattrat(fd, core::ptr::null(), name, value, size, AT_EMPTY_PATH)
}

fn sys_fgetxattrat(
    dirfd: c_int,
    path: *const c_char,
    name: *const c_char,
    value: *mut u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    let name = vm_load_string(name)?;

    debug!("sys_getxattr <= dirfd: {dirfd}, path: {path:?}, name: {name:?}, size: {size}");

    if path.is_none() && flags & AT_EMPTY_PATH != 0 {
        let file_like = get_file_like(dirfd)?;
        let result = if let Some(file) = file_like.downcast_ref::<File>() {
            if size == 0 {
                let mut temp = alloc::vec![0u8; 65536];
                file.getxattr(&name, &mut temp)
                    .map(|len| len as isize)
                    .map_err(noent_to_nodata)?
            } else {
                let mut buffer = alloc::vec![0u8; size];
                let len = file.getxattr(&name, &mut buffer).map_err(noent_to_nodata)?;
                vm_write_slice(value, &buffer[..len])?;
                len as isize
            }
        } else if let Some(dir) = file_like.downcast_ref::<Directory>() {
            if size == 0 {
                let mut temp = alloc::vec![0u8; 65536];
                dir.getxattr(&name, &mut temp)
                    .map(|len| len as isize)
                    .map_err(noent_to_nodata)?
            } else {
                let mut buffer = alloc::vec![0u8; size];
                let len = dir.getxattr(&name, &mut buffer).map_err(noent_to_nodata)?;
                vm_write_slice(value, &buffer[..len])?;
                len as isize
            }
        } else {
            return Err(AxError::OperationNotSupported);
        };
        return Ok(result);
    }

    with_fs(dirfd, |fs| {
        let loc = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(path.as_deref().unwrap_or(""))?
        } else {
            fs.resolve(path.as_deref().unwrap_or(""))?
        };

        if size == 0 {
            let mut temp = alloc::vec![0u8; 65536];
            loc.getxattr(&name, &mut temp)
                .map(|len| len as isize)
                .map_err(noent_to_nodata)
        } else {
            let mut buffer = alloc::vec![0u8; size];
            let len = loc.getxattr(&name, &mut buffer).map_err(noent_to_nodata)?;
            vm_write_slice(value, &buffer[..len])?;
            Ok(len as isize)
        }
    })
}

pub fn sys_setxattr(
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: c_int,
) -> AxResult<isize> {
    sys_fsetxattrat(AT_FDCWD, path, name, value, size, flags as u32, 0)
}

pub fn sys_lsetxattr(
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: c_int,
) -> AxResult<isize> {
    sys_fsetxattrat(
        AT_FDCWD,
        path,
        name,
        value,
        size,
        flags as u32,
        AT_SYMLINK_NOFOLLOW,
    )
}

pub fn sys_fsetxattr(
    fd: c_int,
    name: *const c_char,
    value: *const u8,
    size: usize,
    flags: c_int,
) -> AxResult<isize> {
    sys_fsetxattrat(
        fd,
        core::ptr::null(),
        name,
        value,
        size,
        flags as u32,
        AT_EMPTY_PATH,
    )
}

fn sys_fsetxattrat(
    dirfd: c_int,
    path: *const c_char,
    name: *const c_char,
    value: *const u8,
    size: usize,
    xattr_flags: u32,
    resolve_flags: u32,
) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    let name = vm_load_string(name)?;
    let mut value_buf_uninit: alloc::vec::Vec<core::mem::MaybeUninit<u8>> =
        alloc::vec![core::mem::MaybeUninit::uninit(); size];
    vm_read_slice(value, &mut value_buf_uninit)?;
    let value_buf: alloc::vec::Vec<u8> = unsafe { core::mem::transmute(value_buf_uninit) };

    debug!(
        "sys_setxattr <= dirfd: {dirfd}, path: {path:?}, name: {name:?}, size: {size}, flags: \
         {xattr_flags}"
    );

    if path.is_none() && resolve_flags & AT_EMPTY_PATH != 0 {
        let file_like = get_file_like(dirfd)?;
        if let Some(file) = file_like.downcast_ref::<File>() {
            file.setxattr(&name, &value_buf, xattr_flags)?;
        } else if let Some(dir) = file_like.downcast_ref::<Directory>() {
            dir.setxattr(&name, &value_buf, xattr_flags)?;
        } else {
            return Err(AxError::OperationNotSupported);
        }
        return Ok(0);
    }

    with_fs(dirfd, |fs| {
        let loc = if resolve_flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(path.as_deref().unwrap_or(""))?
        } else {
            fs.resolve(path.as_deref().unwrap_or(""))?
        };
        loc.setxattr(&name, &value_buf, xattr_flags)?;
        Ok(0)
    })
}

pub fn sys_listxattr(path: *const c_char, list: *mut u8, size: usize) -> AxResult<isize> {
    sys_flistxattrat(AT_FDCWD, path, list, size, 0)
}

pub fn sys_llistxattr(path: *const c_char, list: *mut u8, size: usize) -> AxResult<isize> {
    sys_flistxattrat(AT_FDCWD, path, list, size, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_flistxattr(fd: c_int, list: *mut u8, size: usize) -> AxResult<isize> {
    sys_flistxattrat(fd, core::ptr::null(), list, size, AT_EMPTY_PATH)
}

fn sys_flistxattrat(
    dirfd: c_int,
    path: *const c_char,
    list: *mut u8,
    size: usize,
    flags: u32,
) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;

    debug!("sys_listxattr <= dirfd: {dirfd}, path: {path:?}, size: {size}");

    if path.is_none() && flags & AT_EMPTY_PATH != 0 {
        let file_like = get_file_like(dirfd)?;
        let result = if let Some(file) = file_like.downcast_ref::<File>() {
            if size == 0 {
                let mut temp = alloc::vec![0u8; 65536];
                file.listxattr(&mut temp).map(|len| len as isize)?
            } else {
                let mut buffer = alloc::vec![0u8; size];
                let len = file.listxattr(&mut buffer)?;
                vm_write_slice(list, &buffer[..len])?;
                len as isize
            }
        } else if let Some(dir) = file_like.downcast_ref::<Directory>() {
            if size == 0 {
                let mut temp = alloc::vec![0u8; 65536];
                dir.listxattr(&mut temp).map(|len| len as isize)?
            } else {
                let mut buffer = alloc::vec![0u8; size];
                let len = dir.listxattr(&mut buffer)?;
                vm_write_slice(list, &buffer[..len])?;
                len as isize
            }
        } else {
            return Err(AxError::OperationNotSupported);
        };
        return Ok(result);
    }

    with_fs(dirfd, |fs| {
        let loc = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(path.as_deref().unwrap_or(""))?
        } else {
            fs.resolve(path.as_deref().unwrap_or(""))?
        };

        if size == 0 {
            let mut temp = alloc::vec![0u8; 65536];
            loc.listxattr(&mut temp).map(|len| len as isize)
        } else {
            let mut buffer = alloc::vec![0u8; size];
            let len = loc.listxattr(&mut buffer)?;
            vm_write_slice(list, &buffer[..len])?;
            Ok(len as isize)
        }
    })
}

pub fn sys_removexattr(path: *const c_char, name: *const c_char) -> AxResult<isize> {
    sys_fremovexattrat(AT_FDCWD, path, name, 0)
}

pub fn sys_lremovexattr(path: *const c_char, name: *const c_char) -> AxResult<isize> {
    sys_fremovexattrat(AT_FDCWD, path, name, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fremovexattr(fd: c_int, name: *const c_char) -> AxResult<isize> {
    sys_fremovexattrat(fd, core::ptr::null(), name, AT_EMPTY_PATH)
}

fn sys_fremovexattrat(
    dirfd: c_int,
    path: *const c_char,
    name: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    let path = path.nullable().map(vm_load_string).transpose()?;
    let name = vm_load_string(name)?;

    debug!("sys_removexattr <= dirfd: {dirfd}, path: {path:?}, name: {name:?}");

    if path.is_none() && flags & AT_EMPTY_PATH != 0 {
        let file_like = get_file_like(dirfd)?;
        if let Some(file) = file_like.downcast_ref::<File>() {
            file.removexattr(&name).map_err(noent_to_nodata)?;
        } else if let Some(dir) = file_like.downcast_ref::<Directory>() {
            dir.removexattr(&name).map_err(noent_to_nodata)?;
        } else {
            return Err(AxError::OperationNotSupported);
        }
        return Ok(0);
    }

    with_fs(dirfd, |fs| {
        let loc = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            fs.resolve_no_follow(path.as_deref().unwrap_or(""))?
        } else {
            fs.resolve(path.as_deref().unwrap_or(""))?
        };
        loc.removexattr(&name).map_err(noent_to_nodata)?;
        Ok(0)
    })
}
