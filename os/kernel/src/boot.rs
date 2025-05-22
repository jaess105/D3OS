/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: boot                                                            ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Boot sequence of the OS. First Rust function called after       ║
   ║         assembly code: 'start'.                                         ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Fabian Ruhland & Michael Schoettner, HHU                        ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use crate::interrupt::interrupt_dispatcher;
use crate::process::thread::Thread;
use crate::syscall::syscall_dispatcher;
use crate::{
    acpi_tables, allocator, apic, built_info, gdt, init_acpi_tables, init_apic, init_initrd,
    init_pci, init_serial_port, init_terminal, initrd, keyboard, logger, memory, network,
    process_manager, scheduler, serial_port, terminal, timer, tss,
};
use crate::{init_persistent_allocator, naming, persistent_allocator};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use chrono::DateTime;
use uefi::runtime::Time;
use core::arch::x86_64::_rdtsc;
use core::ffi::c_void;
use core::mem::size_of;
use core::ops::Deref;
use core::ptr;
use log::{LevelFilter, debug, info, warn};
use multiboot2::{
    BootInformation, BootInformationHeader, EFIMemoryMapTag, MemoryAreaType, MemoryMapTag,
    TagHeader,
};
use smoltcp::iface;
use smoltcp::iface::Interface;
use smoltcp::time::Instant;
use smoltcp::wire::IpAddress::Ipv4;
use smoltcp::wire::{HardwareAddress, IpCidr, Ipv4Address};
use uefi::data_types::Handle;
use uefi::mem::memory_map::MemoryMap;
use uefi_raw::table::boot::MemoryType;
use uefi_raw::table::system::SystemTable;
use x86_64::PrivilegeLevel::Ring0;
use x86_64::instructions::interrupts;
use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags};
use x86_64::registers::segmentation::SegmentSelector;
use x86_64::structures::gdt::Descriptor;
use x86_64::structures::paging::frame::PhysFrameRange;
use x86_64::structures::paging::page::PageRange;
use x86_64::structures::paging::{Page, PageTable, PageTableFlags, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};

use crate::device::pit::Timer;
use crate::device::ps2::Keyboard;
use crate::device::qemu_cfg;
use crate::device::serial::SerialPort;
use crate::init_cpu_info;
use crate::memory::global_persistent_allocator::{AllocError, GlobalPersistentAllocator};
use crate::memory::nvmem::Nfit;
use crate::memory::pages::page_table_index;
use crate::memory::pool::PoolError;
use crate::memory::vmm::{VirtualMemoryArea, VmaType};
use crate::memory::{MemorySpace, PAGE_SIZE, nvmem};
use crate::network::rtl8139;
use crate::storage;

// import labels from linker script 'link.ld'
unsafe extern "C" {
    static ___KERNEL_DATA_START__: u64; // start address of OS image
    static ___KERNEL_DATA_END__: u64; // end address of OS image
}

const INIT_HEAP_PAGES: usize = 0x400; // number of heap pages for booting the OS

/// First Rust function called from assembly code `boot.asm` \
///   `multiboot2_magic` is the magic number read from 'eax' \
///   and `multiboot2_addr` is the address of multiboot2 info records
#[unsafe(no_mangle)]
pub extern "C" fn start(multiboot2_magic: u32, multiboot2_addr: *const BootInformationHeader) {
    // Initialize logger
    log::set_logger(logger())
        .map(|()| log::set_max_level(LevelFilter::Debug))
        .expect("Failed to initialize logger!");

    // Log messages and panics are now working, but cannot use format string until the heap is initialized later on
    info!("Welcome to D3OS early boot environment!");

    // Get multiboot information
    if multiboot2_magic != multiboot2::MAGIC {
        panic!("Invalid Multiboot2 magic number!");
    }

    // Search memory map, provided by bootloader or EFI, for usable memory and initialize physical memory management with free memory regions
    let multiboot = multiboot2_search_memory_map(multiboot2_addr);

    // Setup the GDT (Global Descriptor Table)
    // Has to be done after EFI boot services have been exited, since they rely on their own GDT
    info!("Initializing GDT");
    init_gdt();

    // The bootloader marks the kernel image region as available, so we need to reserve it manually
    unsafe {
        memory::frames::reserve(kernel_image_region());
    }

    // and initialize kernel heap, after which formatted strings may be used in logs and panics.
    info!("Initializing kernel heap");
    let heap_region = memory::frames::alloc(INIT_HEAP_PAGES);
    unsafe {
        allocator().init(&heap_region);
    }
    debug!(
        "Kernel heap is initialized [0x{:x} - 0x{:x}]",
        heap_region.start.start_address().as_u64(),
        heap_region.end.start_address().as_u64()
    );
    debug!("Page frame allocator:\n{}", memory::frames::dump());

    // Initialize CPU information
    init_cpu_info();

    // Create kernel process (and initialize virtual memory management)
    info!("Create kernel process and initialize paging");
    let kernel_process = process_manager().write().create_process();
    kernel_process.virtual_address_space.load_address_space();

    // Initialize serial port and enable serial logging
    init_serial_port();
    if let Some(serial) = serial_port() {
        logger().register(serial);
    }

    // Map the framebuffer, needed for text output of the terminal
    let fb_info = multiboot
        .framebuffer_tag()
        .expect("No framebuffer information provided by bootloader!")
        .expect("Unknown framebuffer type!");
    let fb_start_page = Page::from_start_address(VirtAddr::new(fb_info.address()))
        .expect("Framebuffer address is not page aligned");
    let fb_end_page = Page::from_start_address(
        VirtAddr::new(fb_info.address() + (fb_info.height() * fb_info.pitch()) as u64)
            .align_up(PAGE_SIZE as u64),
    )
    .unwrap();
    let vma = VirtualMemoryArea::new_with_tag(
        PageRange {
            start: fb_start_page,
            end: fb_end_page,
        },
        VmaType::DeviceMemory,
        "framebuffer",
    );
    kernel_process.virtual_address_space.add_vma(vma);
    kernel_process.virtual_address_space.map(
        vma,
        MemorySpace::Kernel,
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE,
    );

    // Initialize terminal kernel thread and enable terminal logging
    init_terminal(
        fb_info.address() as *mut u8,
        fb_info.pitch(),
        fb_info.width(),
        fb_info.height(),
        fb_info.bpp(),
    );
    // Terminal output uses locks => hangs up when used for debugging
    // MS logger().register(terminal());

    // Dumping basic infos
    info!("Welcome to D3OS!");
    let version = format!(
        "v{} ({} - O{})",
        built_info::PKG_VERSION,
        built_info::PROFILE,
        built_info::OPT_LEVEL
    );
    let git_ref = built_info::GIT_HEAD_REF.unwrap_or("Unknown");
    let git_commit = built_info::GIT_COMMIT_HASH_SHORT.unwrap_or("Unknown");
    let build_date = match DateTime::parse_from_rfc2822(built_info::BUILT_TIME_UTC) {
        Ok(date_time) => date_time.format("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => "Unknown".to_string(),
    };
    let bootloader_name = match multiboot.boot_loader_name_tag() {
        Some(tag) => {
            if tag.name().is_ok() {
                tag.name().unwrap_or("Unknown")
            } else {
                "Unknown"
            }
        }
        None => "Unknown",
    };
    info!("OS Version: [{}]", version);
    info!(
        "Git Version: [{} - {}]",
        built_info::GIT_HEAD_REF.unwrap_or_else(|| "Unknown"),
        git_commit
    );
    info!("Build Date: [{}]", build_date);
    info!("Compiler: [{}]", built_info::RUSTC_VERSION);
    info!("Bootloader: [{}]", bootloader_name);

    // Initialize ACPI tables
    info!("Initializing ACPI tables");
    let rsdp_addr: usize = if let Some(rsdp_tag) = multiboot.rsdp_v2_tag() {
        ptr::from_ref(rsdp_tag) as usize + size_of::<TagHeader>()
    } else if let Some(rsdp_tag) = multiboot.rsdp_v1_tag() {
        ptr::from_ref(rsdp_tag) as usize + size_of::<TagHeader>()
    } else {
        panic!("ACPI not available!");
    };
    init_acpi_tables(rsdp_addr);

    // Initialize interrupts
    info!("Initializing IDT");
    interrupt_dispatcher::setup_idt();
    info!("Initializing system calls");
    syscall_dispatcher::init();
    info!("Initializing APIC");
    init_apic();

    // Initialize timer
    info!("Initializing timer");
    let timer = timer();
    Timer::plugin(Arc::clone(&timer));

    // Enable interrupts
    info!("Enabling interrupts");
    interrupts::enable();

    // Initialize EFI runtime service (if available and not done already during memory initialization)
    if uefi::table::system_table_raw().is_none() {
        match multiboot.efi_sdt64_tag() {
            Some(tag) => {
                info!("Initializing EFI runtime services");
                unsafe { uefi::table::set_system_table(tag.sdt_address() as *const SystemTable) };
            }
            None => warn!("Bootloader did not provide EFI system table pointer"),
        }
    }

    // Dump information about EFI runtime service
    info!(
        "EFI runtime services available (Vendor: [{}], UEFI version: [{}])",
        uefi::system::firmware_vendor(),
        uefi::system::uefi_revision()
    );

    // Initialize keyboard
    info!("Initializing PS/2 devices");
    if let Some(keyboard) = keyboard() {
        Keyboard::plugin(keyboard);
    }

    // Enable serial port interrupts
    if let Some(serial) = serial_port() {
        SerialPort::plugin(serial);
    }

    // Scan PCI bus
    info!("Scanning PCI bus");
    init_pci();

    // Initialize storage devices
    storage::init();

    // Initialize network stack
    network::init();

    // Set up network interface for emulated QEMU network (IP: 10.0.2.15, Gateway: 10.0.2.2)
    if let Some(rtl8139) = rtl8139()
        && qemu_cfg::is_available()
    {
        let time = timer.systime_ms();
        let mut conf = iface::Config::new(HardwareAddress::from(rtl8139.read_mac_address()));
        conf.random_seed = time as u64;

        // The Ssoltcp interface struct wants a mutable reference to the device. However, the RTL8139 driver is designed to work with shared references.
        // Since smoltcp does not actually store the mutable reference anywhere, we can safely cast the shared reference to a mutable one.
        // (Actually, I am not sure why the smoltcp interface wants a mutable reference to the device, since it does not modify the device itself)
        let device = unsafe { ptr::from_ref(rtl8139.deref()).cast_mut().as_mut().unwrap() };
        let mut interface = Interface::new(conf, device, Instant::from_millis(time as i64));
        interface.update_ip_addrs(|ips| {
            ips.push(IpCidr::new(Ipv4(Ipv4Address::new(10, 0, 2, 15)), 24))
                .expect("Failed to add IP address");
        });
        interface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2))
            .expect("Failed to add default route");

        network::add_interface(interface);
    }

    // Initialize non-volatile memory (creates identity mappings for any non-volatile memory regions)
    nvmem::init();

    // As a demo for NVRAM support, we read the last boot time from NVRAM and write the current boot time to it
    if let Ok(nfit) = acpi_tables().lock().find_table::<Nfit>() {
        if let Some(range) = nfit.get_phys_addr_ranges().first() {
            let date_ptr = range.as_phys_frame_range().start.start_address().as_u64() as *mut Time;
            // Read last boot time from NVRAM
            // let date = unsafe { date_ptr.read() };
            // if date.is_valid().is_ok() {
            //     info!("Last boot time: [{:0>4}-{:0>2}-{:0>2} {:0>2}:{:0>2}:{:0>2}]", date.year(), date.month(), date.day(), date.hour(), date.minute(), date.second());
            // }
            //
            // // Write current boot time to NVRAM
            // if efi_services_available() {
            //     if let Ok(time) = uefi::runtime::get_time() {
            //         unsafe { date_ptr.write(time) }
            //     }
            // }

            let nvram_base = range.as_phys_frame_range().start.start_address().as_u64();
            let nvram_size = (range.as_phys_frame_range().end - range.as_phys_frame_range().start)
                as usize
                * PAGE_SIZE;

            let allocator = GlobalPersistentAllocator::new(nvram_base, nvram_size);
            init_persistent_allocator(allocator);

            //Can also be called outside this scope with the exact same line!
            let mut allocator = persistent_allocator().write();

            let pool = allocator.get_or_create_pool(b"POOL1").unwrap();

            match pool.transaction(|tx| {
                //let a = tx.get_by_id::<u64>("data1")?;
                //tx.modify(a, |n| *n += 1)?;
                tx.allocate_with_id("data1", 48879u64)?;
                //Let Qemu crash.
                //If you test this. try the get_by_id function and see that the transaction fails correctly
                //qemu_exit(1);
                Ok(())
            }) {
                Ok(_) => info!("Transaction successful"),
                Err(e) => info!("Transaction failed Correctly: {:?}", e),
            };

            allocator.print_active_pools();

            //run_all_tests(&mut allocator);

            //Test these functions individually
            //-------------------------------
            //test_fragmentation_allocation_overhead(&mut allocator);
            //messure_deallocations(&mut allocator);
            //test_basic_data_types(&mut allocator);
            //measure_performance_time(&mut allocator);
            //test_linked_list(&mut allocator);
            //test_crash_recovery(&mut allocator);

            //*IMPORTANT* Stresstests. USE WITH 4MB FIXED_POOL_SIZE ONLY!
            //-------------------------------
            //test_pool_limits(&mut allocator);
            //test_stress(&mut allocator);

            //Allocates maximum amount Pools - Starting with ID "POOL1"
            //-------------------------------
            //test_full_usage_allocator(&mut allocator);
            let date = unsafe { date_ptr.read() };
            if date.is_valid().is_ok() {
                info!(
                    "Last boot time: [{:0>4}-{:0>2}-{:0>2} {:0>2}:{:0>2}:{:0>2}]",
                    date.year(),
                    date.month(),
                    date.day(),
                    date.hour(),
                    date.minute(),
                    date.second()
                );
            }
        }
    }

    // Init naming service
    naming::api::init();

    // Load initial ramdisk
    let initrd_tag = multiboot
        .module_tags()
        .find(|module| module.cmdline().is_ok_and(|name| name == "initrd"))
        .expect("Initrd not found!");
    init_initrd(initrd_tag);

    // Create and register the cleanup thread in the scheduler
    // (If the last thread of a process terminates, it cannot delete its own address space)
    scheduler().ready(Thread::new_kernel_thread(
        || {
            loop {
                scheduler().sleep(100);
                process_manager().write().drop_exited_process();
            }
        },
        "cleanup",
    ));

    // Create and register the 'shell' thread (from app image in ramdisk) in the scheduler
    scheduler().ready(Thread::load_application(
        initrd()
            .entries()
            .find(|entry| entry.filename().as_str().unwrap() == "shell")
            .expect("Shell application not available!")
            .data(),
        "shell",
        &Vec::new(),
    ));

    // Disable terminal logging (remove terminal output stream)
    logger().remove(terminal().as_ref());
    terminal().clear();

    println!(
        include_str!("banner.txt"),
        version,
        git_ref.rsplit("/").next().unwrap_or(git_ref),
        git_commit,
        build_date,
        built_info::RUSTC_VERSION
            .split_once("(")
            .unwrap_or((built_info::RUSTC_VERSION, ""))
            .0
            .trim(),
        bootloader_name
    );

    // Dump information about all processes (including VMAs)
    process_manager().read().dump();

    // Start APIC timer & scheduler
    info!("Starting scheduler");
    apic().start_timer(10);

    scheduler().start();
}

/// Set up the GDT
fn init_gdt() {
    let mut gdt = gdt().lock();
    let tss = tss().lock();

    gdt.append(Descriptor::kernel_code_segment());
    gdt.append(Descriptor::kernel_data_segment());
    gdt.append(Descriptor::user_data_segment());
    gdt.append(Descriptor::user_code_segment());

    unsafe {
        // We need to obtain a static reference to the TSS and GDT for the following operations.
        // We know, that they have a static lifetime, since they are declared as static variables in 'kernel/mod.rs'.
        // However, since they are hidden behind a Mutex, the borrow checker does not see them with a static lifetime.
        let gdt_ref = ptr::from_ref(gdt.deref()).as_ref().unwrap();
        let tss_ref = ptr::from_ref(tss.deref()).as_ref().unwrap();
        gdt.append(Descriptor::tss_segment(tss_ref));
        gdt_ref.load();
    }

    unsafe {
        // Load task state segment
        load_tss(SegmentSelector::new(5, Ring0));

        // Set code and stack segment register
        CS::set_reg(SegmentSelector::new(1, Ring0));
        SS::set_reg(SegmentSelector::new(2, Ring0));

        // Other segment registers are not used in long mode (set to 0)
        DS::set_reg(SegmentSelector::new(0, Ring0));
        ES::set_reg(SegmentSelector::new(0, Ring0));
        FS::set_reg(SegmentSelector::new(0, Ring0));
        GS::set_reg(SegmentSelector::new(0, Ring0));
    }
}

/// Return `PhysFrameRange` for memory occupied by the kernel image
fn kernel_image_region() -> PhysFrameRange {
    let start: PhysFrame;
    let end: PhysFrame;

    unsafe {
        start = PhysFrame::from_start_address(PhysAddr::new(
            ptr::from_ref(&___KERNEL_DATA_START__) as u64,
        ))
        .expect("Kernel code is not page aligned");
        end = PhysFrame::from_start_address(
            PhysAddr::new(ptr::from_ref(&___KERNEL_DATA_END__) as u64).align_up(PAGE_SIZE as u64),
        )
        .unwrap();
    }

    return PhysFrameRange { start, end };
}

/// Identifies usable memory and initialize physical memory management \
/// and returns `BootInformation` by searching the memory maps, provided by bootloader of EFI. \
///   `multiboot2_addr` is the address of multiboot2 info records
fn multiboot2_search_memory_map(
    multiboot2_addr: *const BootInformationHeader,
) -> BootInformation<'static> {
    let multiboot = unsafe {
        BootInformation::load(multiboot2_addr).expect("Failed to get Multiboot2 information")
    };

    // Search memory map, provided by bootloader of EFI, for usable memory and initialize physical memory management
    if let Some(_) = multiboot.efi_bs_not_exited_tag() {
        // EFI boot services have not been exited, and we obtain access to the memory map and EFI runtime services by exiting them manually
        info!("EFI boot services have not been exited yet");
        let image_tag = multiboot
            .efi_ih64_tag()
            .expect("EFI image handle not available!");
        let sdt_tag = multiboot
            .efi_sdt64_tag()
            .expect("EFI system table not available!");
        let memory_map;

        unsafe {
            let image_handle = Handle::from_ptr(image_tag.image_handle() as *mut c_void)
                .expect("Failed to create EFI image handle struct from pointer!");
            uefi::table::set_system_table(sdt_tag.sdt_address() as *const SystemTable);
            uefi::boot::set_image_handle(image_handle);

            info!("Exiting EFI boot services to obtain runtime system table and memory map");
            memory_map = uefi::boot::exit_boot_services(MemoryType::LOADER_DATA);
        }

        scan_efi_memory_map(&memory_map);
    } else {
        info!("EFI boot services have already been exited by the bootloader");
        if let Some(memory_map) = multiboot.efi_memory_map_tag() {
            // EFI services have been exited, but the bootloader has provided us with the EFI memory map
            info!("Bootloader provides EFI memory map");
            scan_efi_multiboot2_memory_map(memory_map);
        } else if let Some(memory_map) = multiboot.memory_map_tag() {
            // EFI services have been exited, but the bootloader has provided us with a Multiboot2 memory map
            info!("Bootloader provides Multiboot2 memory map");
            scan_multiboot2_memory_map(memory_map);
        } else {
            panic!("No memory information available!");
        }
    }
    multiboot
}

/// Searching available memory regions provided by multiboot2 in `memory map`. \
/// Available only if efi boot services have been exited and bootloader provides these memory maps.
fn scan_multiboot2_memory_map(memory_map: &MemoryMapTag) {
    info!("Searching memory map for available regions");
    memory_map
        .memory_areas()
        .iter()
        .filter(|area| area.typ() == MemoryAreaType::Available)
        .for_each(|area| unsafe {
            memory::frames::insert(PhysFrameRange {
                start: PhysFrame::from_start_address(
                    PhysAddr::new(area.start_address()).align_up(PAGE_SIZE as u64),
                )
                .unwrap(),
                end: PhysFrame::from_start_address(
                    PhysAddr::new(area.end_address()).align_down(PAGE_SIZE as u64),
                )
                .unwrap(),
            });
        });
}

/// Memory map from efi. Only available if boot services have been exited. \
/// Sometimes bootloaders do not provide multiboot2 memory maps if \
/// efi information has been requested.
fn scan_efi_multiboot2_memory_map(memory_map: &EFIMemoryMapTag) {
    info!("Searching memory map for available regions");
    memory_map
        .memory_areas()
        .filter(|area| {
            area.ty.0 == MemoryType::CONVENTIONAL.0
                || area.ty.0 == MemoryType::LOADER_CODE.0
                || area.ty.0 == MemoryType::LOADER_DATA.0
                || area.ty.0 == MemoryType::BOOT_SERVICES_CODE.0
                || area.ty.0 == MemoryType::BOOT_SERVICES_DATA.0
        }) // .0 necessary because of different version dependencies to uefi-crate
        .for_each(|area| {
            let start = PhysFrame::from_start_address(
                PhysAddr::new(area.phys_start).align_up(PAGE_SIZE as u64),
            )
            .unwrap();
            let frames = PhysFrame::range(start, start + area.page_count);

            // Non-conventional memory may be write-protected, and we need to unprotect it first
            if area.ty.0 != MemoryType::CONVENTIONAL.0 {
                unprotect_frames(frames);
            }

            unsafe {
                memory::frames::insert(frames);
            }
        });
}

/// Memory map from efi. Only available if boot services have NOT been exited.
fn scan_efi_memory_map(memory_map: &dyn MemoryMap) {
    info!("Searching memory map for available regions");
    memory_map
        .entries()
        .filter(|area| {
            area.ty == MemoryType::CONVENTIONAL
                || area.ty == MemoryType::LOADER_CODE
                || area.ty == MemoryType::LOADER_DATA
                || area.ty == MemoryType::BOOT_SERVICES_CODE
                || area.ty == MemoryType::BOOT_SERVICES_DATA
        })
        .for_each(|area| {
            let start = PhysFrame::from_start_address(
                PhysAddr::new(area.phys_start).align_up(PAGE_SIZE as u64),
            )
            .unwrap();
            let frames = PhysFrame::range(start, start + area.page_count);

            // Non-conventional memory may be write-protected, and we need to unprotect it first
            if area.ty != MemoryType::CONVENTIONAL {
                unprotect_frames(frames);
            }

            unsafe {
                memory::frames::insert(frames);
            }
        });
}

fn unprotect_frames(frames: PhysFrameRange) {
    unsafe { Cr0::update(|flags| flags.remove(Cr0Flags::WRITE_PROTECT)) };

    let root_level = if Cr4::read().contains(Cr4Flags::L5_PAGING) {
        5
    } else {
        4
    };
    for frame in frames {
        unprotect_frame(frame, root_level);
    }

    unsafe { Cr0::update(|flags| flags.insert(Cr0Flags::WRITE_PROTECT)) };
}

fn unprotect_frame(frame: PhysFrame, root_level: usize) {
    let addr = VirtAddr::new(frame.start_address().as_u64());
    let mut page_table = unsafe {
        (Cr3::read().0.start_address().as_u64() as *mut PageTable)
            .as_mut()
            .unwrap()
    };

    let mut level = root_level;
    loop {
        let index = page_table_index(addr, level);
        let entry = &mut page_table[index];
        let flags = entry.flags();

        if level == 1 || flags.contains(PageTableFlags::HUGE_PAGE) {
            entry.set_flags(flags | PageTableFlags::WRITABLE);
            break;
        }

        page_table = unsafe { (entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
        level -= 1;
    }
}

#[derive(Copy, Clone)]
struct SmallObject {
    id: u32,
    active: bool,
}

#[derive(Copy, Clone)]
struct MediumObject {
    id: u64,
    name: [u8; 32],
    data: [u8; 256],
}

#[derive(Copy, Clone)]
struct LargeObject {
    id: u64,
    data: [u8; 1024 * 4], // 4KB
}

// Create a large object that's almost 1MB
#[repr(C)]
#[derive(Copy, Clone)]
struct HugeObject {
    id: u64,
    data: [u8; 1000 * 64], // less than 1MB ,object table always takes 0x10040 bytes-> so we have less than 2MB
}

// Main test runner
fn run_all_tests(allocator: &mut GlobalPersistentAllocator) {
    test_single_pool(allocator);
    test_multiple_pools(allocator);
    test_basic_data_types(allocator);
    test_memory_pressure(allocator);
    test_type_safety(allocator);
    test_crash_recovery(allocator);
    test_fragmentation_allocation_overhead(allocator);
    test_linked_list(allocator);
    test_list_modifications(allocator);
    test_list_stress(allocator);
    //ONLY call this test set the FIXED_POOL_SIZE to 4MB
    //test_pool_limits(allocator);
    measure_performance_time(allocator);
    messure_deallocations(allocator);

    info!("All tests and measurement completed successfully!");
}

// Test Scenarios
fn test_single_pool(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Single Pool Operations ===");
    let pool = allocator.get_or_create_pool(b"TEST_POOL").unwrap();

    // 1. Basic Operations
    info!("Test 1: Basic Operations");
    pool.transaction(|tx| {
        tx.allocate_with_id(
            "small1",
            SmallObject {
                id: 1,
                active: true,
            },
        )?;
        tx.allocate_with_id(
            "medium1",
            MediumObject {
                id: 1,
                name: *b"TestObject\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
                data: [0; 256],
            },
        )?;
        tx.allocate_with_id(
            "large1",
            LargeObject {
                id: 1,
                data: [0; 1024 * 4],
            },
        )?;
        tx.deallocate_by_id("small1")?;
        tx.allocate_with_id(
            "small2",
            SmallObject {
                id: 2,
                active: false,
            },
        )?;
        tx.allocate_with_id(
            "small1",
            SmallObject {
                id: 123,
                active: true,
            },
        )?;
        Ok(())
    })
    .expect("Basic operations test failed");

    //verify
    pool.transaction(|tx| {
        let small1 = tx.read_by_id::<SmallObject>("small1")?;
        let small2 = tx.read_by_id::<SmallObject>("small2")?;
        let medium1 = tx.read_by_id::<MediumObject>("medium1")?;
        let large1 = tx.read_by_id::<LargeObject>("large1")?;

        assert_eq!(small1.id, 123);
        assert_eq!(small1.active, true);
        assert_eq!(small2.id, 2);
        assert_eq!(small2.active, false);
        assert_eq!(medium1.id, 1);
        assert_eq!(large1.id, 1);
        Ok(())
    })
    .expect("Basic operations verification failed");
}

fn test_multiple_pools(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Multiple Pools ===");

    // Test Pool 1
    {
        let pool1 = allocator.get_or_create_pool(b"POOL1").unwrap();
        pool1
            .transaction(|tx| {
                tx.allocate_with_id("pool1_data", 42u64)?;
                Ok(())
            })
            .expect("Pool 1 test failed");
    }

    // Test Pool 2
    {
        let pool2 = allocator.get_or_create_pool(b"POOL2").unwrap();
        pool2
            .transaction(|tx| {
                tx.allocate_with_id("pool2_data", 84u64)?;
                Ok(())
            })
            .expect("Pool 2 test failed");
    }
}

fn test_basic_data_types(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Basic Data Types Storage ===");
    let pool = allocator.get_or_create_pool(b"BASIC_TYPES").unwrap();

    // Test string storage
    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    struct PersistentString {
        length: usize,
        data: [u8; 64], // Fixed size buffer
    }

    pool.transaction(|tx| {
        // Store string
        let test_str = "Hello, Persistent Memory!";
        let mut pers_str = PersistentString {
            length: test_str.len(),
            data: [0; 64],
        };
        pers_str.data[..test_str.len()].copy_from_slice(test_str.as_bytes());

        tx.allocate_with_id("test_string", pers_str)?;
        info!("Stored string successfully");
        Ok(())
    })
    .expect("Failed to store string");

    // Read string back
    pool.transaction(|tx| {
        let pers_str = tx.read_by_id::<PersistentString>("test_string")?;
        let recovered_str =
            core::str::from_utf8(&pers_str.data[..pers_str.length]).expect("Invalid UTF-8");
        info!("Recovered string: {}", recovered_str);
        Ok(())
    })
    .expect("Failed to read string");

    // Test different numeric types
    pool.transaction(|tx| {
        // Integers
        tx.allocate_with_id("int8", -42i8)?;
        tx.allocate_with_id("uint8", 42u8)?;
        tx.allocate_with_id("int16", -1234i16)?;
        tx.allocate_with_id("uint16", 1234u16)?;
        tx.allocate_with_id("int32", -123456i32)?;
        tx.allocate_with_id("uint32", 123456u32)?;
        tx.allocate_with_id("int64", -123456789i64)?;
        tx.allocate_with_id("uint64", 123456789u64)?;

        // Boolean
        tx.allocate_with_id("bool_true", true)?;
        tx.allocate_with_id("bool_false", false)?;

        // Character
        tx.allocate_with_id("char", 'R')?;

        info!("Stored all numeric types successfully");
        Ok(())
    })
    .expect("Failed to store numeric types");

    // Read read types
    pool.transaction(|tx| {
        // Read integers
        info!("int8: {}", tx.read_by_id::<i8>("int8")?);
        info!("uint8: {}", tx.read_by_id::<u8>("uint8")?);
        info!("int16: {}", tx.read_by_id::<i16>("int16")?);
        info!("uint16: {}", tx.read_by_id::<u16>("uint16")?);
        info!("int32: {}", tx.read_by_id::<i32>("int32")?);
        info!("uint32: {}", tx.read_by_id::<u32>("uint32")?);
        info!("int64: {}", tx.read_by_id::<i64>("int64")?);
        info!("uint64: {}", tx.read_by_id::<u64>("uint64")?);

        // Read boolean
        info!("bool_true: {}", tx.read_by_id::<bool>("bool_true")?);
        info!("bool_false: {}", tx.read_by_id::<bool>("bool_false")?);
        assert_eq!(tx.read_by_id::<bool>("bool_true")?, true);
        assert_eq!(tx.read_by_id::<bool>("bool_false")?, false);
        // Read character
        info!("char: {}", tx.read_by_id::<char>("char")?);

        Ok(())
    })
    .expect("Failed to read numeric types");

    // Test array storage
    pool.transaction(|tx| {
        // Store array
        let array = [1, 2, 3, 4, 5];
        tx.allocate_with_id("array", array)?;

        // Store fixed-size matrix
        let matrix = [[1, 2, 3], [4, 5, 6], [7, 8, 9]];
        tx.allocate_with_id("matrix", matrix)?;

        info!("Stored arrays successfully");
        Ok(())
    })
    .expect("Failed to store arrays");

    // Read arrays back
    pool.transaction(|tx| {
        let array = tx.read_by_id::<[i32; 5]>("array")?;
        info!("Recovered array: {:?}", array);

        let matrix = tx.read_by_id::<[[i32; 3]; 3]>("matrix")?;
        info!("Recovered matrix: {:?}", matrix);
        Ok(())
    })
    .expect("Failed to read arrays");

    // Test crash recovery with string
    pool.transaction(|tx| {
        let test_str = "This string should survive a crash!";
        let mut pers_str = PersistentString {
            length: test_str.len(),
            data: [0; 64],
        };
        pers_str.data[..test_str.len()].copy_from_slice(test_str.as_bytes());

        tx.allocate_with_id("crash_string", pers_str)?;
        info!("Stored string before crash");

        Err::<(), PoolError>(PoolError::TransactionFailed)
    })
    .expect_err("Transaction should fail");

    //should not exist
    pool.transaction(|tx| {
        if let Ok(pers_str) = tx.read_by_id::<PersistentString>("crash_string") {
            panic!("String should not exist after crash {:?}", pers_str);
        }
        Ok(())
    })
    .expect("Failed to verify after crash");
}

fn test_memory_pressure(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Memory Pressure ===");
    let pool = allocator.get_or_create_pool(b"PRESSURE_TEST").unwrap();

    pool.transaction(|tx| {
        for i in 0..10 {
            tx.allocate_with_id(
                &format!("large{}", i),
                LargeObject {
                    id: i as u64,
                    data: [i as u8; 1024 * 4],
                },
            )?;
        }
        Ok(())
    })
    .expect("Memory pressure test failed");

    //pool.debug_print_object_table();
}

fn test_type_safety(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Type Safety ===");
    let pool = allocator.get_or_create_pool(b"TYPE_SAFETY").unwrap();

    pool.transaction(|tx| {
        tx.allocate_with_id("type_test", 42u64)?;
        Ok(())
    })
    .expect("Type safety test failed");

    pool.transaction(|tx| {
        // This should fail with type mismatch
        match tx.get_by_id::<u32>("type_test") {
            Err(PoolError::TypeMismatch { .. }) => info!("Type safety check passed"),
            Err(e) => info!("Type safety check failed: {:?}", e),
            _ => info!("Type safety check failed"),
        }
        Ok(())
    })
    .expect("Type safety test failed");
    //pool.debug_print_object_table();
}

fn measure_performance_time(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Performance Tests ===");

    // Measure pool creation
    let start1 = unsafe { _rdtsc() };
    let pool = allocator.get_or_create_pool(b"PERF_TEST").unwrap();
    let end1 = unsafe { _rdtsc() };
    info!("Pool creation: {} tsc", end1 - start1);

    // Measure single small allocation

    pool.transaction(|tx| {
        let start2 = unsafe { _rdtsc() };
        tx.allocate_with_id("single", 42u64)?;
        let end2 = unsafe { _rdtsc() };
        info!("Single allocation 8bytes : {} tsc", end2 - start2);
        Ok(())
    })
    .expect("Single allocation failed");

    // Measure bulk allocations
    pool.transaction(|tx| {
        let start3 = unsafe { _rdtsc() };
        for i in 0..100 {
            tx.allocate_with_id(&format!("bulk{}", i), i as u64)?;
        }
        let end3 = unsafe { _rdtsc() };
        info!(
            "100 allocations: {} tsc (avg: {} tsc per allocation)",
            end3 - start3,
            (end3 - start3) as f64 / 100.0
        );
        Ok(())
    })
    .expect("Bulk allocation failed");

    pool.transaction(|tx| {
        let start5 = unsafe { _rdtsc() };
        for i in 0..200 {
            tx.allocate_with_id(&format!("bulk{}", i), i as u64)?;
        }
        let end5 = unsafe { _rdtsc() };
        info!(
            "200 allocations: {} tsc (avg: {} tsc per allocation)",
            end5 - start5,
            (end5 - start5) as f64 / 100.0
        );
        Ok(())
    })
    .expect("Bulk allocation failed");

    pool.transaction(|tx| {
        let start6 = unsafe { _rdtsc() };
        for i in 0..500 {
            tx.allocate_with_id(&format!("bulk{}", i), i as u64)?;
        }
        let end6 = unsafe { _rdtsc() };
        info!(
            "500 allocations: {} tsc (avg: {} tsc per allocation)",
            end6 - start6,
            (end6 - start6) as f64 / 100.0
        );
        Ok(())
    })
    .expect("Bulk allocation failed");

    // Measure large allocation
    pool.transaction(|tx| {
        let start4 = unsafe { _rdtsc() };
        tx.allocate_with_id(
            "large",
            LargeObject {
                id: 1,
                data: [0; 4096],
            },
        )?;
        let end4 = unsafe { _rdtsc() };
        info!("4KB allocation: {} tsc", end4 - start4);
        Ok(())
    })
    .expect("Large allocation failed");

    //Compare with KernelAlloc
    let start7 = unsafe { _rdtsc() };
    let _ = Box::new(1u64);

    let end7 = unsafe { _rdtsc() };
    info!("KernelAlloc 8bytes: {} tsc", end7 - start7);

    let start8 = unsafe { _rdtsc() };
    for i in 0..100 {
        let _ = Box::new(i as u64);
    }
    let end8 = unsafe { _rdtsc() };
    info!(
        "KernelAlloc 100 allocations: {} tsc (avg: {} tsc per allocation)",
        end8 - start8,
        (end8 - start8) as f64 / 100.0
    );

    let start9 = unsafe { _rdtsc() };
    for i in 0..200 {
        let _ = Box::new(i as u64);
    }
    let end9 = unsafe { _rdtsc() };
    info!(
        "KernelAlloc 200 allocations: {} tsc (avg: {} tsc per allocation)",
        end9 - start9,
        (end9 - start9) as f64 / 100.0
    );

    let start10 = unsafe { _rdtsc() };
    for i in 0..500 {
        let _ = Box::new(i as u64);
    }
    let end10 = unsafe { _rdtsc() };
    info!(
        "KernelAlloc 500 allocations: {} tsc (avg: {} tsc per allocation)",
        end10 - start10,
        (end10 - start10) as f64 / 100.0
    );

    let start11 = unsafe { _rdtsc() };
    let _ = Box::new([0u8; 4096]);
    let end11 = unsafe { _rdtsc() };
    info!("KernelAlloc 4KB: {} tsc", end11 - start11);
}

fn messure_deallocations(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Deallocation ===");
    let pool = allocator.get_or_create_pool(b"DEALLOCATION_TEST").unwrap();

    // 1. Basic Operations
    info!("8byte deallocation");
    pool.transaction(|tx| {
        tx.allocate_with_id(
            "small1",
            SmallObject {
                id: 1,
                active: true,
            },
        )?;
        let start1 = unsafe { _rdtsc() };
        tx.deallocate_by_id("small1")?;
        let end1 = unsafe { _rdtsc() };
        info!("8byte deallocation: {} tsc", end1 - start1);
        Ok(())
    })
    .expect("Deallocation failed");

    info!("4KB deallocation");
    pool.transaction(|tx| {
        tx.allocate_with_id(
            "large1",
            LargeObject {
                id: 1,
                data: [1; 4096],
            },
        )?;
        let start2 = unsafe { _rdtsc() };
        tx.deallocate_by_id("large1")?;
        let end2 = unsafe { _rdtsc() };
        info!("4KB deallocation: {} tsc", end2 - start2);
        Ok(())
    })
    .expect("Deallocation failed");

    info!("100 8byte deallocations");
    pool.transaction(|tx| {
        for i in 0..100 {
            tx.allocate_with_id(
                &format!("small{}", i),
                SmallObject {
                    id: i as u32,
                    active: true,
                },
            )?;
        }
        let start3 = unsafe { _rdtsc() };
        for i in 0..100 {
            tx.deallocate_by_id(&format!("small{}", i))?;
        }
        let end3 = unsafe { _rdtsc() };
        info!(
            "100 8byte deallocations: {} tsc (avg: {} tsc per deallocation)",
            end3 - start3,
            (end3 - start3) as f64 / 100.0
        );
        Ok(())
    })
    .expect("Deallocation failed");

    info!("200 8byte deallocations");
    pool.transaction(|tx| {
        for i in 0..200 {
            tx.allocate_with_id(
                &format!("small{}", i),
                SmallObject {
                    id: i as u32,
                    active: true,
                },
            )?;
        }
        let start4 = unsafe { _rdtsc() };
        for i in 0..200 {
            tx.deallocate_by_id(&format!("small{}", i))?;
        }
        let end4 = unsafe { _rdtsc() };
        info!(
            "200 8byte deallocations: {} tsc (avg: {} tsc per deallocation)",
            end4 - start4,
            (end4 - start4) as f64 / 200.0
        );
        Ok(())
    })
    .expect("Deallocation failed");

    info!("500 8byte deallocations");
    pool.transaction(|tx| {
        for i in 0..500 {
            tx.allocate_with_id(
                &format!("small{}", i),
                SmallObject {
                    id: i as u32,
                    active: true,
                },
            )?;
        }
        let start5 = unsafe { _rdtsc() };
        for i in 0..500 {
            tx.deallocate_by_id(&format!("small{}", i))?;
        }
        let end5 = unsafe { _rdtsc() };
        info!(
            "500 8byte deallocations: {} tsc (avg: {} tsc per deallocation)",
            end5 - start5,
            (end5 - start5) as f64 / 500.0
        );
        Ok(())
    })
    .expect("Deallocation failed");

    info!("Now for Kernelheap");

    let test1 = Box::new(1u64);
    let start6 = unsafe { _rdtsc() };
    drop(test1);
    let end6 = unsafe { _rdtsc() };
    info!("KernelAlloc 8bytes: {} tsc", end6 - start6);

    let test2 = Box::new([0u8; 4096]);
    let kb64start = unsafe { _rdtsc() };
    drop(test2);
    let kb64end = unsafe { _rdtsc() };
    info!("KernelAlloc 4KB: {} tsc", kb64end - kb64start);

    // Test 100 deallocations
    {
        let boxes: Vec<Box<u64>> = (0..100).map(|i| Box::new(i)).collect();
        let start = unsafe { _rdtsc() };
        drop(boxes);
        let end = unsafe { _rdtsc() };
        info!(
            "KernelDealloc 100 deallocations: {} tsc (avg: {} tsc per deallocation)",
            end - start,
            (end - start) as f64 / 100.0
        );
    }

    // Test 200 deallocations
    {
        let boxes: Vec<Box<u64>> = (0..200).map(|i| Box::new(i)).collect();
        let start = unsafe { _rdtsc() };
        drop(boxes);
        let end = unsafe { _rdtsc() };
        info!(
            "KernelDealloc 200 deallocations: {} tsc (avg: {} tsc per deallocation)",
            end - start,
            (end - start) as f64 / 200.0
        );
    }

    // Test 500 deallocations
    {
        let boxes: Vec<Box<u64>> = (0..500).map(|i| Box::new(i)).collect();
        let start = unsafe { _rdtsc() };
        drop(boxes);
        let end = unsafe { _rdtsc() };
        info!(
            "KernelDealloc 500 deallocations: {} tsc (avg: {} tsc per deallocation)",
            end - start,
            (end - start) as f64 / 500.0
        );
    }
}

fn test_fragmentation_allocation_overhead(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Fragmentation Allocation Overhead ===");
    let pool = allocator.get_or_create_pool(b"FRAG_OVERHEAD").unwrap();

    // Different sized objects
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct SmallBlock {
        id: u64,
        data: [u8; 16 * 1000], // 16KB
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct MediumBlock {
        id: u64,
        data: [u8; 32 * 1000], // 32KB
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct LargeBlock {
        id: u64,
        data: [u8; 64 * 1000], // 64KB
    }

    // 1. Create initial fragmented state
    info!("Step 1: Creating initial fragmented state");
    // Calculate overhead
    info!("=== Performance Analysis ===");
    info!("Memory layout before final allocation:");
    info!("- Large block (64KB)");
    info!("- Free space (16KB) - First hole");
    info!("- Large block (64KB)");
    info!("- Free space (32KB) - Second hole");
    info!("- Large block (64KB)");
    info!("- Free space (16KB) - Third hole");

    pool.transaction(|tx| {
        // Allocate in pattern: Large, Small, Large, Medium, Large, Small
        let start = unsafe { _rdtsc() };
        tx.allocate_with_id(
            "large_1",
            LargeBlock {
                id: 1,
                data: [1; 64 * 1000],
            },
        )?;
        tx.allocate_with_id(
            "small_1",
            SmallBlock {
                id: 2,
                data: [2; 16 * 1000],
            },
        )?;
        tx.allocate_with_id(
            "large_2",
            LargeBlock {
                id: 3,
                data: [3; 64 * 1000],
            },
        )?;
        tx.allocate_with_id(
            "medium_1",
            MediumBlock {
                id: 4,
                data: [4; 32 * 1000],
            },
        )?;
        tx.allocate_with_id(
            "large_3",
            LargeBlock {
                id: 5,
                data: [5; 64 * 1000],
            },
        )?;
        tx.allocate_with_id(
            "small_2",
            SmallBlock {
                id: 6,
                data: [6; 16 * 1000],
            },
        )?;
        let end = unsafe { _rdtsc() };
        info!("Time to create initial state: {} tsc", end - start);
        Ok(())
    })
    .expect("Initial allocation failed");

    info!("Initial state created");

    // 2. Create fragmentation by deallocating specific blocks
    info!("Step 2: Creating fragmentation");

    pool.transaction(|tx| {
        // Delete some blocks to create fragmented free space
        let start = unsafe { _rdtsc() };
        tx.deallocate_by_id("small_1")?; // Creates 16KB hole
        tx.deallocate_by_id("medium_1")?; // Creates 32KB hole
        tx.deallocate_by_id("small_2")?; // Creates 16KB hole
        let end = unsafe { _rdtsc() };
        info!(
            "Time to create 64 kb(16 , 32, 16) fragmentation: {} tsc",
            end - start
        );
        Ok(())
    })
    .expect("Deallocation failed");

    // pool.debug_print_object_table();
    info!("Fragmentation created");

    // 3. Try to allocate a block that won't fit in first free space
    info!("Step 3: Allocating block that needs to skip first free space");
    pool.transaction(|tx| {
        // Try to allocate a medium block (32KB) - should skip the first 16KB hole
        let start = unsafe { _rdtsc() };
        tx.allocate_with_id(
            "medium_new",
            MediumBlock {
                id: 7,
                data: [7; 32 * 1000],
            },
        )?;
        let end = unsafe { _rdtsc() };
        info!("Time to allocate with fragmentation: {} tsc", end - start);
        Ok(())
    })
    .expect("New allocation failed");

    info!("Final state after allocation");

    // 4. Compare with allocation in clean pool
    info!("Step 4: Comparing with allocation in clean pool");
    let clean_pool = allocator.get_or_create_pool(b"CLEAN_POOL").unwrap();

    clean_pool
        .transaction(|tx| {
            let start = unsafe { _rdtsc() };
            tx.allocate_with_id(
                "medium_clean",
                MediumBlock {
                    id: 8,
                    data: [8; 32 * 1000],
                },
            )?;
            let end = unsafe { _rdtsc() };
            info!("Time to allocate in clean pool: {} tsc", end - start);
            Ok(())
        })
        .expect("Clean allocation failed");
}

//FOR ME ONLY

fn test_full_usage_allocator(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Full Usage of Allocator ===");
    let mut i = 1;
    loop {
        match allocator.get_or_create_pool(format!("POOL{i}").as_bytes()) {
            Ok(_) => i += 1,
            Err(e) => match e {
                AllocError::NoPoolsAvailable => {
                    info!("No more pools available");
                    break;
                }
                _ => {
                    panic!("Error: {:?}", e);
                }
            },
        }
    }
}

/// IMPORTANT: **Call this fn only if pools have exactly 4MB of POOL_SIZE**
/// Description: Test pool Storage Limits
///
///
///
fn test_pool_limits(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Pool Storage Limits ===");

    //Test 1: Maximum number of small objects (1024)
    info!("Test 1: Maximum number of objects (1024 small objects)");
    {
        let pool = allocator.get_or_create_pool(b"MAX_OBJECTS").unwrap();

        // Try to allocate 1024 small objects (should succeed)
        pool.transaction(|tx| {
            for i in 0..1024 {
                tx.allocate_with_id(
                    &format!("small_{}", i),
                    SmallObject {
                        id: i as u32,
                        active: true,
                    },
                )?;
            }
            info!("Successfully allocated 1024 objects");
            Ok(())
        })
        .expect("Failed to allocate 1024 objects");

        // Try to allocate one more object (should fail)
        match pool.transaction(|tx| {
            tx.allocate_with_id(
                "one_too_many",
                SmallObject {
                    id: 1025,
                    active: true,
                },
            )?;
            Ok(())
        }) {
            Err(PoolError::ObjectTableFull) => info!("Successfully caught object limit overflow"),
            Ok(_) => panic!("Expected failure on 1025th object, but it succeeded"),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    info!("Test 2: Maximum size limit (4,128,704 bytes)");
    {
        let pool = allocator.get_or_create_pool(b"MAX_SIZE").unwrap();

        info!("Pool created successfully");
        //pool.debug_print_object_table();

        info!(
            "HugeObject size: {} bytes",
            core::mem::size_of::<HugeObject>()
        );

        // We need about 64 objects to reach ~4MB (64 * 64KB = 4MB)
        // Let's allocate them in chunks to test different scenarios

        // First chunk (16 objects = ~1MB)
        match pool.transaction(|tx| {
            info!("Allocating first chunk (16 objects, ~1MB)...");
            for i in 0..16 {
                tx.allocate_with_id(
                    &format!("huge_{}", i),
                    HugeObject {
                        id: i as u64,
                        data: [i as u8; 1000 * 64],
                    },
                )?;
            }
            info!("First chunk allocated successfully");
            Ok(())
        }) {
            Ok(_) => info!("First 1MB allocation completed successfully"),
            Err(e) => panic!("First chunk allocation failed: {:?}", e),
        }

        // Second chunk (16 objects = ~1MB)
        match pool.transaction(|tx| {
            info!("Allocating second chunk (16 objects, ~1MB)...");
            for i in 16..32 {
                tx.allocate_with_id(
                    &format!("huge_{}", i),
                    HugeObject {
                        id: i as u64,
                        data: [i as u8; 1000 * 64],
                    },
                )?;
            }
            info!("Second chunk allocated successfully");
            Ok(())
        }) {
            Ok(_) => info!("Second 1MB allocation completed successfully"),
            Err(e) => panic!("Second chunk allocation failed: {:?}", e),
        }

        // Third chunk (16 objects = ~1MB)
        match pool.transaction(|tx| {
            info!("Allocating third chunk (16 objects, ~1MB)...");
            for i in 32..48 {
                tx.allocate_with_id(
                    &format!("huge_{}", i),
                    HugeObject {
                        id: i as u64,
                        data: [i as u8; 1000 * 64],
                    },
                )?;
            }
            info!("Third chunk allocated successfully");
            Ok(())
        }) {
            Ok(_) => info!("Third 1MB allocation completed successfully"),
            Err(e) => panic!("Third chunk allocation failed: {:?}", e),
        }

        // Fourth chunk (16 objects = ~1MB)
        match pool.transaction(|tx| {
            info!("Allocating fourth chunk (16 objects, ~1MB)...");
            for i in 48..64 {
                tx.allocate_with_id(
                    &format!("huge_{}", i),
                    HugeObject {
                        id: i as u64,
                        data: [i as u8; 1000 * 64],
                    },
                )?;
            }
            info!("Fourth chunk allocated successfully");
            Ok(())
        }) {
            Ok(_) => info!("Fourth 1MB allocation completed successfully"),
            Err(e) => panic!("Fourth chunk allocation failed: {:?}", e),
        }

        //allocate the rest of the memory -> 4128704 - 4*16*64*1000 = 32704

        #[repr(C)]
        #[derive(Copy, Clone)]
        struct SpecialObject {
            id: u64,
            data: [u8; 32127],
        }

        match pool.transaction(|tx| {
            info!("Allocating rest of the memory...");

            tx.allocate_with_id(
                &format!("huge_65"),
                SpecialObject {
                    id: 10 as u64,
                    data: [1 as u8; 32127],
                },
            )?;

            info!("Rest of the memory allocated successfully");
            Ok(())
        }) {
            Ok(_) => info!("Rest of the memory allocation completed successfully"),
            Err(e) => panic!("Rest of the memory allocation failed: {:?}", e),
        }

        // Verify all allocations
        match pool.transaction(|tx| {
            info!("Verifying all allocations...");
            for i in 0..64 {
                match tx.read_by_id::<HugeObject>(&format!("huge_{}", i)) {
                    Ok(obj) => {
                        assert_eq!(obj.id, i as u64, "Object ID mismatch for huge_{}", i);
                        assert_eq!(obj.data[0], i as u8, "Data mismatch for huge_{}", i);
                    }
                    Err(e) => panic!("Failed to verify object huge_{}: {:?}", i, e),
                }
            }
            info!("All allocations verified successfully");
            Ok(())
        }) {
            Ok(_) => info!("Verification completed successfully"),
            Err(e) => panic!("Verification failed: {:?}", e),
        }

        // Try to allocate one more object (should fail as we're at the limit)
        match pool.transaction(|tx| {
            info!("Attempting to allocate beyond limit...");
            tx.allocate_with_id(
                "huge_overflow",
                HugeObject {
                    id: 64,
                    data: [64; 1000 * 64],
                },
            )?;
            Ok(())
        }) {
            Err(PoolError::AllocationFailed) => info!("Successfully caught size limit overflow"),
            Ok(_) => panic!("Expected failure on overflow allocation, but it succeeded"),
            Err(e) => panic!("Unexpected error on overflow allocation: {:?}", e),
        }

        //pool.debug_print_object_table();
        info!("Test 2 completed successfully");
    }
}

fn test_crash_recovery(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Crash Recovery ===");
    let pool = allocator.get_or_create_pool(b"RECOVERY_TEST").unwrap();
    //
    // 1. Test recovery after allocation crash
    info!("Test 1: Recovery after allocation crash");
    pool.transaction(|tx| {
        tx.allocate_with_id(
            "recover1",
            SmallObject {
                id: 1,
                active: true,
            },
        )?;
        // Simulate crash by returning error
        Err::<(), PoolError>(PoolError::TransactionFailed)
    })
    .expect_err("Transaction should fail");

    // Verify recovery
    pool.transaction(|tx| {
        match tx.get_by_id::<SmallObject>("recover1") {
            Err(PoolError::InvalidId) => info!("Recovery successful - object properly rolled back"),
            Ok(_) => panic!("Recovery failed - object still exists after rollback"),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
        Ok(())
    })
    .expect("Recovery verification failed");

    //2. Test recovery after modification crash
    info!("Test 2: Recovery after modification crash");
    pool.transaction(|tx| {
        tx.allocate_with_id(
            "recover2",
            SmallObject {
                id: 2,
                active: false,
            },
        )?;
        Ok(())
    })
    .expect("Initial allocation failed");

    pool.transaction(|tx| {
        let ptr = tx.get_by_id::<SmallObject>("recover2")?;
        tx.modify(ptr, |obj| obj.active = true)?;
        // Simulate crash during modification
        //qemu_exit(123);
        Err::<(), PoolError>(PoolError::TransactionFailed)
    })
    .expect_err("Transaction should fail");

    info!("Verify recovery");

    //Verify recovery
    pool.transaction(|tx| {
        let obj = tx.read_by_id::<SmallObject>("recover2")?;
        assert!(
            !obj.active,
            "Recovery failed - modification persisted after rollback"
        );
        info!("obj status: {}", obj.active);
        info!("Recovery successful - modification properly rolled back");
        Ok(())
    })
    .expect("Recovery verification failed");

    //pool.debug_print_object_table();

    // 3. Test recovery after deallocation crash
    info!("Test 3: Recovery after deallocation crash");
    pool.transaction(|tx| {
        tx.allocate_with_id(
            "recover3",
            MediumObject {
                id: 3,
                name: *b"TestObject\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
                data: [0; 256],
            },
        )?;
        Ok(())
    })
    .expect("Initial allocation failed");

    pool.transaction(|tx| {
        tx.deallocate_by_id("recover3")?;
        // Simulate crash during deallocation
        Err::<(), PoolError>(PoolError::TransactionFailed)
    })
    .expect_err("Transaction should fail");

    // Verify recovery
    pool.transaction(|tx| {
        match tx.get_by_id::<MediumObject>("recover3") {
            Ok(_) => info!("Recovery successful - deallocation properly rolled back"),
            Err(e) => info!("Recovery failed - object not found after rollback: {:?}", e),
        }
        Ok(())
    })
    .expect("Recovery verification failed");

    //pool.debug_print_object_table();

    // 4. Test recovery of multiple operations
    info!("Test 4: Recovery of multiple operations");
    pool.transaction(|tx| {
        // Multiple operations in one transaction
        tx.allocate_with_id(
            "multi1",
            SmallObject {
                id: 4,
                active: true,
            },
        )?;
        let ptr = tx.allocate_with_id(
            "multi2",
            SmallObject {
                id: 5,
                active: false,
            },
        )?;
        tx.modify(ptr, |obj| obj.active = true)?;
        tx.deallocate_by_id("multi1")?;
        // Simulate crash

        Err::<(), PoolError>(PoolError::TransactionFailed)
    })
    .expect_err("Transaction should fail");

    //pool.debug_print_object_table();
    //pool.debug_log_pool_state();

    // Verify complete rollback
    pool.transaction(|tx| {
        assert!(
            tx.get_by_id::<SmallObject>("multi1").is_err(),
            "Recovery failed - multi1 exists"
        );
        assert!(
            tx.get_by_id::<SmallObject>("multi2").is_err(),
            "Recovery failed - multi2 exists"
        );
        info!("Recovery successful - all operations properly rolled back");
        Ok(())
    })
    .expect("Recovery verification failed");
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ListNode {
    value: u64,
    next_offset: u64, // Offset from pool base, 0 means no next node
    prev_offset: u64, // Offset from pool base, 0 means no prev node
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct LinkedList {
    head_offset: u64, // Offset from pool base to first node
    tail_offset: u64, // Offset from pool base to last node
    size: usize,
}

impl LinkedList {
    pub fn new() -> Self {
        LinkedList {
            head_offset: 0,
            tail_offset: 0,
            size: 0,
        }
    }
}

fn test_linked_list(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Persistent Linked List ===");
    let pool = allocator.get_or_create_pool(b"LIST_TEST").unwrap();
    let base_address = pool.base_address;

    // 1. Initialize list
    pool.transaction(|tx| {
        let list = LinkedList::new();
        tx.allocate_with_id("list", list)?;
        info!("List initialized");
        Ok(())
    })
    .expect("Failed to initialize list");

    // // 2. Add nodes
    let start1 = unsafe { _rdtsc() };
    for i in 1..=5 {
        pool.transaction(|tx| {
            let mut list = tx.read_by_id::<LinkedList>("list")?;

            let new_node = ListNode {
                value: i,
                next_offset: 0,
                prev_offset: list.tail_offset,
            };

            // Allocate new node
            let node_ptr = tx.allocate_with_id(&format!("node_{}", i), new_node)?;
            let node_offset = (node_ptr.as_ptr() as u64) - base_address;

            // Update list
            if list.size == 0 {
                list.head_offset = node_offset;
            } else {
                // Update previous tail's next pointer
                let tail_ptr = tx.get_by_id::<ListNode>(&format!("node_{}", i - 1))?;
                tx.modify(tail_ptr, |node| node.next_offset = node_offset)?;
            }

            list.tail_offset = node_offset;
            list.size += 1;

            // Update list metadata
            let list_ptr = tx.get_by_id::<LinkedList>("list")?;
            tx.modify(list_ptr, |l| *l = list)?;

            info!("Added node {} to list", i);
            Ok(())
        })
        .expect("Failed to add node");
    }
    let end1 = unsafe { _rdtsc() };
    info!("Adding 5 nodes: {} tsc", end1 - start1);

    // 4. Print list (after recovery)
    pool.transaction(|tx| {
        let list = tx.read_by_id::<LinkedList>("list")?;
        info!("List after recovery - size: {}", list.size);

        let mut current_offset = list.head_offset;
        while current_offset != 0 {
            let mut node_found = false;
            for i in 1..=list.size {
                if let Ok(node) = tx.read_by_id::<ListNode>(&format!("node_{}", i)) {
                    if (tx.get_by_id::<ListNode>(&format!("node_{}", i))?.as_ptr() as u64)
                        - base_address
                        == current_offset
                    {
                        info!(
                            "Node {}: value={}, next={:#x}, prev={:#x}",
                            i, node.value, node.next_offset, node.prev_offset
                        );
                        current_offset = node.next_offset;
                        node_found = true;
                        break;
                    }
                }
            }
            if !node_found {
                break;
            }
        }
        Ok(())
    })
    .expect("Failed to print list");
}

// Additional test cases:
fn test_list_modifications(allocator: &mut GlobalPersistentAllocator) {
    let pool = allocator.get_or_create_pool(b"LIST_MOD").unwrap();

    // 1. Create and populate list
    pool.transaction(|tx| {
        let list = LinkedList::new();
        tx.allocate_with_id("mod_list", list)?;
        Ok(())
    })
    .expect("Failed to create list");

    // 2. Test node modification with crash
    pool.transaction(|tx| {
        let node = ListNode {
            value: 42,
            next_offset: 0,
            prev_offset: 0,
        };
        tx.allocate_with_id("test_node", node)?;

        // Modify and crash
        let ptr = tx.get_by_id::<ListNode>("test_node")?;
        tx.modify(ptr, |n| n.value = 100)?;
        //qemu_exit(124);
        Ok(())
    })
    .ok();

    // 3. Verify recovery
    pool.transaction(|tx| {
        if let Ok(node) = tx.read_by_id::<ListNode>("test_node") {
            info!("Node value after recovery: {}", node.value);
        }
        Ok(())
    })
    .expect("Failed to verify recovery");
}

fn test_list_stress(allocator: &mut GlobalPersistentAllocator) {
    let pool = allocator.get_or_create_pool(b"LIST_STRESS").unwrap();

    // Add many nodes quickly
    for i in 0..100 {
        pool.transaction(|tx| {
            let node = ListNode {
                value: i,
                next_offset: 0,
                prev_offset: 0,
            };
            tx.allocate_with_id(&format!("stress_node_{}", i), node)?;
            Ok(())
        })
        .expect("Failed stress test allocation");
    }

    // Modify nodes randomly
    for _ in 0..50 {
        let idx = unsafe { _rdtsc() % 100 };

        pool.transaction(|tx| {
            if let Ok(ptr) = tx.get_by_id::<ListNode>(&format!("stress_node_{}", idx)) {
                tx.modify(ptr, |n| n.value += 1000)?;
            }
            Ok(())
        })
        .expect("Failed stress test modification");
    }
}

//Only used for the stress test in thesis use more than 2MB for this!
fn test_stress(allocator: &mut GlobalPersistentAllocator) {
    info!("=== Testing Edge Cases ===");
    let pool = allocator.get_or_create_pool(b"EDGE_CASES").unwrap();

    //1. Alloc Delloc multiply times

    for i in 0..100 {
        //let start1 = unsafe { _rdtsc() };
        pool.transaction(|tx| {
            for i in 0..512 {
                tx.allocate_with_id(
                    &format!("alloc{}", i),
                    SmallObject {
                        id: i,
                        active: true,
                    },
                )?;
                tx.deallocate_by_id(&format!("alloc{}", i))?;
            }
            Ok(())
        })
        .expect("Allocation deallocation test failed");
        let end1 = unsafe { _rdtsc() };
        //info!("{}. Alloc Delloc 512 times: {} tsc", i,end1 - start1);
    }
    info!("Alloc Delloc 512 times: Done");
}
