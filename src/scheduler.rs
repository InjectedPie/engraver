extern crate pbr;
extern crate rayon;

use chan;
use gpu_hasher::create_gpu_hasher_thread;
use libc::{c_void, size_t, uint64_t};
#[cfg(feature = "opencl")]
use ocl::noncegen_gpu;
use ocl::GpuContext;
use plotter::{Buffer, PlotterTask};
use std::cmp::min;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread;

const CPU_TASK_SIZE: u64 = 64;

const NONCE_SIZE: u64 = (2 << 17);

extern "C" {
    pub fn noncegen(
        cache: *mut c_void,
        cache_size: size_t,
        chunk_offset: size_t,
        numeric_ID: uint64_t,
        local_startnonce: uint64_t,
        local_nonces: uint64_t,
    );
    pub fn noncegen_sse(
        cache: *mut c_void,
        cache_size: size_t,
        chunk_offset: size_t,
        numeric_ID: uint64_t,
        local_startnonce: uint64_t,
        local_nonces: uint64_t,
    );
    pub fn noncegen_avx(
        cache: *mut c_void,
        cache_size: size_t,
        chunk_offset: size_t,
        numeric_ID: uint64_t,
        local_startnonce: uint64_t,
        local_nonces: uint64_t,
    );
    pub fn noncegen_avx2(
        cache: *mut c_void,
        cache_size: size_t,
        chunk_offset: size_t,
        numeric_ID: uint64_t,
        local_startnonce: uint64_t,
        local_nonces: uint64_t,
    );
    pub fn noncegen_avx512(
        cache: *mut c_void,
        cache_size: size_t,
        chunk_offset: size_t,
        numeric_ID: uint64_t,
        local_startnonce: uint64_t,
        local_nonces: uint64_t,
    );
}
pub struct SafeCVoid {
    ptr: *mut c_void,
}
unsafe impl Send for SafeCVoid {}

pub struct SafePointer {
    ptr: *mut u8,
}
unsafe impl Send for SafePointer {}

pub struct HasherTaskInfo {
    pub cache: SafeCVoid,
    pub cache_size: size_t,
    pub chunk_offset: size_t,
    pub numeric_id: uint64_t,
    pub local_startnonce: uint64_t,
    pub local_nonces: uint64_t,
}

pub struct GPUHasherTaskInfo {
    pub cache: SafePointer,
    pub cache_size: size_t,
    pub chunk_offset: size_t,
    pub numeric_id: uint64_t,
    pub local_startnonce: uint64_t,
    pub local_nonces: uint64_t,
}

pub fn hash(
    tx: Sender<(u8, u8, u64)>,
    hasher_task: HasherTaskInfo,
    simd_ext: String,
) -> impl FnOnce() {
    move || {
        unsafe {
            match &*simd_ext {
                "AVX512F" => noncegen_avx512(
                    hasher_task.cache.ptr,
                    hasher_task.cache_size,
                    hasher_task.chunk_offset,
                    hasher_task.numeric_id,
                    hasher_task.local_startnonce,
                    hasher_task.local_nonces,
                ),
                "AVX2" => noncegen_avx2(
                    hasher_task.cache.ptr,
                    hasher_task.cache_size,
                    hasher_task.chunk_offset,
                    hasher_task.numeric_id,
                    hasher_task.local_startnonce,
                    hasher_task.local_nonces,
                ),
                "AVX" => noncegen_avx(
                    hasher_task.cache.ptr,
                    hasher_task.cache_size,
                    hasher_task.chunk_offset,
                    hasher_task.numeric_id,
                    hasher_task.local_startnonce,
                    hasher_task.local_nonces,
                ),
                "SSE2" => noncegen_sse(
                    hasher_task.cache.ptr,
                    hasher_task.cache_size,
                    hasher_task.chunk_offset,
                    hasher_task.numeric_id,
                    hasher_task.local_startnonce,
                    hasher_task.local_nonces,
                ),
                _ => noncegen(
                    hasher_task.cache.ptr,
                    hasher_task.cache_size,
                    hasher_task.chunk_offset,
                    hasher_task.numeric_id,
                    hasher_task.local_startnonce,
                    hasher_task.local_nonces,
                ),
            }
        }
        tx.send((0u8, 0u8, hasher_task.local_nonces))
            .expect("Pool task can't communicate with hasher thread.");
    }
}

// currently a thread, will be changed to async task
#[cfg(feature = "opencl")]
pub fn hash_gpu(
    tx: Sender<(u8, u8, u64)>,
    hasher_task: GPUHasherTaskInfo,
    gpu_context: Arc<GpuContext>,
) -> impl FnOnce() {
    move || {
        noncegen_gpu(
            hasher_task.cache.ptr,
            hasher_task.cache_size,
            hasher_task.chunk_offset,
            hasher_task.numeric_id,
            hasher_task.local_startnonce,
            hasher_task.local_nonces,
            gpu_context,
        );
        tx.send((1u8, 0u8, hasher_task.local_nonces))
            .expect("Pool task can't communicate with hasher thread.");
    }
}

pub fn create_scheduler_thread(
    task: Arc<PlotterTask>,
    thread_pool: rayon::ThreadPool,
    mut nonces_hashed: u64,
    mut pb: Option<pbr::ProgressBar<pbr::Pipe>>,
    rx_empty_buffers: chan::Receiver<Buffer>,
    tx_buffers_to_writer: chan::Sender<Buffer>,
    simd_ext: String,
    gpu_contexts: Option<Vec<Arc<GpuContext>>>,
) -> impl FnOnce() {
    move || {
        // synchronisation chanel for all hashing devices (CPU+GPU)
        // message protocol:    (hash_device_id: u8, message: u8, nonces processed: u64)
        // hash_device_id:      0=CPU, 1=GPU0, 2=GPU1...
        // message:             0 = data ready to write
        //                      1 = device ready to compute next hashing batch
        // nonces_processed:    nonces hashed / nonces writen to host buffer
        let (tx, rx) = channel();

        // create gpu threads and channels
        let gpus = gpu_contexts.unwrap();
        let mut gpu_threads = Vec::new();
        let mut gpu_channels = Vec::new();
        for (i, gpu) in gpus.iter().enumerate() {
            gpu_channels.push(chan::unbounded());
            gpu_threads.push(thread::spawn({
                create_gpu_hasher_thread(
                    (i + 1) as u8,
                    gpu.clone(),
                    tx.clone(),
                    gpu_channels.last().unwrap().1.clone(),
                )
            }));
        }

        for buffer in rx_empty_buffers {
            let gpu_task_size: u64 = gpus[0].worksize as u64;
            let mut_bs = &buffer.get_buffer();
            let mut bs = mut_bs.lock().unwrap();
            let buffer_size = (*bs).len() as u64;
            let nonces_to_hash = min(buffer_size / NONCE_SIZE, task.nonces - nonces_hashed);

            let mut n_jobs = nonces_to_hash as usize / gpu_task_size as usize;
            if nonces_to_hash % gpu_task_size > 0 {
                n_jobs += 1;
            }

            for j in 0..nonces_to_hash / gpu_task_size {
                let task = hash_gpu(
                    tx.clone(),
                    GPUHasherTaskInfo {
                        cache: SafePointer {
                            ptr: bs.as_mut_ptr(),
                        },
                        cache_size: buffer_size / NONCE_SIZE,
                        chunk_offset: j * gpu_task_size,
                        numeric_id: task.numeric_id,
                        local_startnonce: task.start_nonce + nonces_hashed + j * gpu_task_size,
                        local_nonces: gpu_task_size,
                    },
                    gpus[0].clone()
                    //simd_ext.clone(),
                );

                thread_pool.spawn(task);
            }

            // hash remainder
            if nonces_to_hash % gpu_task_size > 0 {
                let task = hash_gpu(
                    tx.clone(),
                    GPUHasherTaskInfo {
                        cache: SafePointer {
                            ptr: bs.as_mut_ptr(),
                        },
                        cache_size: buffer_size / NONCE_SIZE,
                        chunk_offset: nonces_to_hash / gpu_task_size * gpu_task_size,
                        numeric_id: task.numeric_id,
                        local_startnonce: task.start_nonce
                            + nonces_hashed
                            + nonces_to_hash / gpu_task_size * gpu_task_size,
                        local_nonces: nonces_to_hash % gpu_task_size,
                    },
                    gpus[0].clone()
                    //simd_ext.clone(),
                );
                thread_pool.spawn(task);
            }

            // sync pool and push status to progressbar
            assert_eq!(
                rx.iter().take(n_jobs).fold(0, |a, b| {
                    match &mut pb {
                        Some(pb) => {
                            pb.add(b.2 * 1024 * 256);
                        }
                        None => (),
                    }
                    a + b.2
                }),
                nonces_to_hash
            );

            nonces_hashed += nonces_to_hash;

            // queue buffer for writing
            tx_buffers_to_writer.send(buffer);

            // thread end
            if task.nonces == nonces_hashed {
                match &mut pb {
                    Some(pb) => {
                        pb.finish_print("Hasher done.");
                    }
                    None => (),
                }
                // shutdown gpu threads
                for gpu in gpu_channels.iter() {
                    gpu.0.send(None);
                }
                break;
            };
        }
    }
}