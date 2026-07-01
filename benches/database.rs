// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
#[path = "common/db_store.rs"]
mod db_store;

use db_store::{
	configured_backends, payment_details_batch, payment_key, payment_update_batch_from_offset,
	pending_payment_details_batch_from_offset, pending_payment_update_batch_from_offset, Backend,
	PaginatedStoreFixture, StoreFixture, BATCH_LEN, PAGINATED_PAGE_LEN,
};

fn database_benchmark(c: &mut Criterion) {
	let backends = configured_backends();

	benchmark_payment_store_single_ops(c, &backends);
	benchmark_payment_store_warm_sequential(c, &backends);
	benchmark_payment_store_concurrent(c, &backends);
	benchmark_payment_store(c, &backends);
	benchmark_payment_store_paginated(c, &backends);
	benchmark_payment_store_lifecycle(c, &backends);
	benchmark_pending_payment_store(c, &backends);
}

fn benchmark_payment_store(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/payment_store");
	group.throughput(Throughput::Elements(BATCH_LEN));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		let insert_payments = payment_details_batch(0);
		group.bench_with_input(
			BenchmarkId::new("insert_100_sequential_cold", backend.name()),
			&backend,
			|b, backend| {
				let insert_payments = insert_payments.clone();
				b.to_async(runtime.as_ref()).iter_custom(|iters| {
					let backend = *backend;
					let insert_payments = insert_payments.clone();
					async move {
						let mut elapsed = Duration::ZERO;
						for _ in 0..iters {
							let fixture = StoreFixture::new(backend, "payment_insert").await;
							let start = Instant::now();
							fixture.write_payment_batch(insert_payments.clone()).await;
							elapsed += start.elapsed();
						}
						elapsed
					}
				})
			},
		);

		let update_payments = payment_details_batch(0);
		let updates = payment_update_batch_from_offset(0);
		group.bench_with_input(
			BenchmarkId::new("update_100_sequential_cold", backend.name()),
			&backend,
			|b, backend| {
				let update_payments = update_payments.clone();
				let updates = updates.clone();
				b.to_async(runtime.as_ref()).iter_custom(|iters| {
					let backend = *backend;
					let update_payments = update_payments.clone();
					let updates = updates.clone();
					async move {
						let mut elapsed = Duration::ZERO;
						for _ in 0..iters {
							let fixture = StoreFixture::new(backend, "payment_update").await;
							fixture.write_payment_batch(update_payments.clone()).await;
							let start = Instant::now();
							fixture.write_payment_update_batch(updates.clone()).await;
							elapsed += start.elapsed();
						}
						elapsed
					}
				})
			},
		);

		let reload_payments = payment_details_batch(0);
		group.bench_with_input(
			BenchmarkId::new("reload_100_cold", backend.name()),
			&backend,
			|b, backend| {
				let reload_payments = reload_payments.clone();
				b.to_async(runtime.as_ref()).iter_custom(|iters| {
					let backend = *backend;
					let reload_payments = reload_payments.clone();
					async move {
						let mut elapsed = Duration::ZERO;
						for _ in 0..iters {
							let fixture = StoreFixture::new(backend, "payment_reload").await;
							fixture.write_payment_batch(reload_payments.clone()).await;
							let start = Instant::now();
							let payments = fixture.reload_payments().await;
							elapsed += start.elapsed();
							// Keep the read result observable in optimized benchmark builds.
							std::hint::black_box(payments);
						}
						elapsed
					}
				})
			},
		);
	}
	group.finish();
}

fn benchmark_payment_store_paginated(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/payment_store_paginated");
	group.throughput(Throughput::Elements(PAGINATED_PAGE_LEN));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		let Some(fixture) = runtime.block_on(PaginatedStoreFixture::new(
			backend,
			"payment_list_page",
			Arc::clone(&runtime),
		)) else {
			continue;
		};
		let fixture = Arc::new(fixture);

		let runner = Arc::clone(&runtime);
		let first_page_fixture = Arc::clone(&fixture);
		group.bench_function(BenchmarkId::new("list_page_from_10k", backend.name()), |b| {
			b.to_async(runner.as_ref()).iter_custom(|iters| {
				let first_page_fixture = Arc::clone(&first_page_fixture);
				async move {
					let mut elapsed = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						let page_len = first_page_fixture.list_first_page().await;
						elapsed += start.elapsed();
						debug_assert_eq!(page_len, PAGINATED_PAGE_LEN as usize);
						// Keep the page result observable when debug assertions are compiled out.
						std::hint::black_box(page_len);
					}
					elapsed
				}
			})
		});
		let runner = Arc::clone(&runtime);
		let second_page_fixture = Arc::clone(&fixture);
		group.bench_function(BenchmarkId::new("list_second_page_from_10k", backend.name()), |b| {
			b.to_async(runner.as_ref()).iter_custom(|iters| {
				let second_page_fixture = Arc::clone(&second_page_fixture);
				async move {
					let mut elapsed = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						let page_len = second_page_fixture.list_second_page().await;
						elapsed += start.elapsed();
						debug_assert_eq!(page_len, PAGINATED_PAGE_LEN as usize);
						// Keep the page result observable when debug assertions are compiled out.
						std::hint::black_box(page_len);
					}
					elapsed
				}
			})
		});
	}
	group.finish();
}

fn benchmark_pending_payment_store(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/pending_payment_store");
	group.throughput(Throughput::Elements(BATCH_LEN));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		let insert_payments = pending_payment_details_batch_from_offset(0);
		group.bench_with_input(
			BenchmarkId::new("insert_100_sequential_cold", backend.name()),
			&backend,
			|b, backend| {
				let insert_payments = insert_payments.clone();
				b.to_async(runtime.as_ref()).iter_custom(|iters| {
					let backend = *backend;
					let insert_payments = insert_payments.clone();
					async move {
						let mut elapsed = Duration::ZERO;
						for _ in 0..iters {
							let fixture =
								StoreFixture::new(backend, "pending_payment_insert").await;
							let start = Instant::now();
							fixture.write_pending_payment_batch(insert_payments.clone()).await;
							elapsed += start.elapsed();
						}
						elapsed
					}
				})
			},
		);

		let update_payments = pending_payment_details_batch_from_offset(0);
		let updates = pending_payment_update_batch_from_offset(0);
		group.bench_with_input(
			BenchmarkId::new("update_100_sequential_cold", backend.name()),
			&backend,
			|b, backend| {
				let update_payments = update_payments.clone();
				let updates = updates.clone();
				b.to_async(runtime.as_ref()).iter_custom(|iters| {
					let backend = *backend;
					let update_payments = update_payments.clone();
					let updates = updates.clone();
					async move {
						let mut elapsed = Duration::ZERO;
						for _ in 0..iters {
							let fixture =
								StoreFixture::new(backend, "pending_payment_update").await;
							fixture.write_pending_payment_batch(update_payments.clone()).await;
							let start = Instant::now();
							fixture.write_pending_payment_update_batch(updates.clone()).await;
							elapsed += start.elapsed();
						}
						elapsed
					}
				})
			},
		);
	}
	group.finish();
}

fn benchmark_payment_store_single_ops(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/payment_store_single");
	group.throughput(Throughput::Elements(1));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		group.bench_function(BenchmarkId::new("write_new_key", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture = StoreFixture::new(backend, "single_write_new_key").await;
				let mut elapsed = Duration::ZERO;
				for idx in 0..iters {
					let start = Instant::now();
					fixture.write_payment(idx).await;
					elapsed += start.elapsed();
				}
				elapsed
			})
		});

		group.bench_function(BenchmarkId::new("write_existing_key", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture = StoreFixture::new(backend, "single_update_existing_key").await;
				fixture.write_payment(0).await;
				let mut elapsed = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					fixture.write_payment_update(0).await;
					elapsed += start.elapsed();
				}
				elapsed
			})
		});

		group.bench_function(BenchmarkId::new("read_existing_key", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture = StoreFixture::new(backend, "single_read_existing_key").await;
				fixture.write_payment(0).await;
				let mut elapsed = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					let payment = fixture.read_payment(0).await;
					elapsed += start.elapsed();
					// Keep the read result observable in optimized benchmark builds.
					std::hint::black_box(payment);
				}
				elapsed
			})
		});

		group.bench_with_input(
			BenchmarkId::new("remove_existing_key", backend.name()),
			&backend,
			|b, backend| {
				b.to_async(runtime.as_ref()).iter_custom(|iters| {
					let backend = *backend;
					async move {
						let mut elapsed = Duration::ZERO;
						for idx in 0..iters {
							let fixture =
								StoreFixture::new(backend, "single_remove_existing_key").await;
							fixture.write_payment(idx).await;
							let key = payment_key(idx);
							let start = Instant::now();
							fixture.remove_payment_key(&key).await;
							elapsed += start.elapsed();
						}
						elapsed
					}
				})
			},
		);
	}
	group.finish();
}

fn benchmark_payment_store_warm_sequential(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/payment_store_warm");
	group.throughput(Throughput::Elements(BATCH_LEN));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		group.bench_function(BenchmarkId::new("insert_100_sequential", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture =
					StoreFixture::new(backend, "payment_insert_100_sequential_warm").await;
				let mut elapsed = Duration::ZERO;
				for iter in 0..iters {
					let offset = iter * BATCH_LEN;
					let start = Instant::now();
					fixture.write_payment_batch_from_offset(offset).await;
					elapsed += start.elapsed();
				}
				elapsed
			})
		});
	}
	group.finish();
}

fn benchmark_payment_store_concurrent(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/payment_store_concurrent");
	group.throughput(Throughput::Elements(BATCH_LEN));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		group.bench_function(BenchmarkId::new("insert_100_distinct_keys", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture =
					StoreFixture::new(backend, "payment_insert_100_concurrent_distinct").await;
				let mut elapsed = Duration::ZERO;
				for iter in 0..iters {
					let offset = iter * BATCH_LEN;
					let start = Instant::now();
					fixture.write_payment_batch_concurrent(offset, false).await;
					elapsed += start.elapsed();
				}
				elapsed
			})
		});

		group.bench_function(BenchmarkId::new("insert_100_same_key", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture =
					StoreFixture::new(backend, "payment_insert_100_concurrent_same_key").await;
				let mut elapsed = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					fixture.write_payment_batch_concurrent(0, true).await;
					elapsed += start.elapsed();
				}
				elapsed
			})
		});
	}
	group.finish();
}

fn benchmark_payment_store_lifecycle(c: &mut Criterion, backends: &[Backend]) {
	let mut group = c.benchmark_group("database/payment_store_lifecycle");
	group.throughput(Throughput::Elements(1));
	let runtime = benchmark_runtime();
	for backend in backends.iter().copied() {
		group.bench_function(BenchmarkId::new("insert_update_read", backend.name()), |b| {
			b.to_async(runtime.as_ref()).iter_custom(|iters| async move {
				let fixture = StoreFixture::new(backend, "payment_lifecycle").await;
				let mut elapsed = Duration::ZERO;
				for idx in 0..iters {
					let start = Instant::now();
					let payment = fixture.insert_update_read_payment(idx).await;
					elapsed += start.elapsed();
					// Keep the read result observable in optimized benchmark builds.
					std::hint::black_box(payment);
				}
				elapsed
			})
		});
	}
	group.finish();
}

fn benchmark_runtime() -> Arc<tokio::runtime::Runtime> {
	let mut builder = tokio::runtime::Builder::new_multi_thread();
	builder.worker_threads(2).enable_all();
	#[cfg(tokio_unstable)]
	builder.enable_eager_driver_handoff();
	Arc::new(builder.build().unwrap())
}

criterion_group!(benches, database_benchmark);
criterion_main!(benches);
