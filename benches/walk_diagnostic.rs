//! Fixed-work probe: no Criterion calibration in hardware counter runs.
//! Reuses the comparison adapters verbatim; setup/destruction are outside time
//! but inside external perf counters (use two iteration counts to subtract).
#[allow(dead_code)]
mod adapters {
    include!("comparison.rs");

    type ChainProbe<E> = (
        E,
        <E as Engine>::Signal,
        Rc<Cell<u64>>,
        Rc<Cell<usize>>,
        Rc<Cell<usize>>,
    );

    fn chain_probe<E: Engine + 'static, const COUNT: bool>(
        size: usize,
        cut: bool,
    ) -> ChainProbe<E> {
        let e = E::new();
        let input = e.signal(0);
        let memos = Rc::new(Cell::new(0));
        let effects = Rc::new(Cell::new(0));
        let mut tail = if COUNT {
            let calls = memos.clone();
            e.memo(move |e| {
                calls.set(calls.get() + 1);
                let n = e.get(input);
                if cut { 1 } else { n + 1 }
            })
        } else if cut {
            e.memo(move |e| {
                e.get(input);
                1
            })
        } else {
            e.memo(move |e| e.get(input) + 1)
        };
        for _ in 1..size {
            let previous = tail;
            tail = if COUNT {
                let calls = memos.clone();
                e.memo(move |e| {
                    calls.set(calls.get() + 1);
                    e.read(previous) + 1
                })
            } else {
                // Do NOT capture an unused counter in timed memo payloads.
                e.memo(move |e| e.read(previous) + 1)
            };
        }
        let output = Rc::new(Cell::new(0));
        let captured = output.clone();
        if COUNT {
            let calls = effects.clone();
            e.effect(move |e| {
                calls.set(calls.get() + 1);
                captured.set(e.read(tail));
            });
        } else {
            e.effect(move |e| captured.set(e.read(tail)));
        }
        e.settle();
        (e, input, output, memos, effects)
    }

    fn probe<E: Engine + 'static>(shape: &str, size: usize, iterations: u64) -> serde_json::Value {
        // Counted, untimed preflight on an independent graph.
        if matches!(shape, "chain" | "cut") {
            let cut = shape == "cut";
            let (e, input, output, memos, effects) = chain_probe::<E, true>(size, cut);
            for value in 1..=4 {
                memos.set(0);
                effects.set(0);
                e.set(input, value);
                e.settle();
                assert_eq!(memos.get(), if cut { 1 } else { size });
                assert_eq!(effects.get(), usize::from(!cut));
                assert_eq!(output.get(), size as u64 + if cut { 0 } else { value });
            }
        }
        let elapsed = if matches!(shape, "chain" | "cut") {
            let cut = shape == "cut";
            let (e, input, output, _, _) = chain_probe::<E, false>(size, cut);
            for value in 1..=16 {
                e.set(input, value);
                e.settle();
            }
            e.reset_work();
            let start = std::time::Instant::now();
            for value in 17..iterations + 17 {
                e.set(input, black_box(value));
                e.settle();
                black_box(output.get());
            }
            let elapsed = start.elapsed();
            assert_eq!(
                output.get(),
                size as u64 + if cut { 0 } else { iterations + 16 }
            );
            (elapsed, e.work())
        } else {
            let shape = match shape {
                "diamond" => Shape::Diamond,
                "fan_out" => Shape::FanOut,
                "conditional" => Shape::Conditional,
                _ => panic!("unknown shape"),
            };
            let preflight = Graph::<E>::new::<true>(shape, size);
            preflight.validate();
            drop(preflight);
            let graph = Graph::<E>::new::<false>(shape, size);
            for value in 1..=16 {
                graph.step(value);
            }
            graph.engine.reset_work();
            let start = std::time::Instant::now();
            for value in 17..iterations + 17 {
                black_box(graph.step(black_box(value)));
            }
            let elapsed = start.elapsed();
            assert_eq!(graph.output.get(), graph.expected(iterations + 16));
            (elapsed, graph.engine.work())
        };
        serde_json::json!({"engine":E::NAME,"shape":shape,"size":size,"iterations":iterations,
            "elapsed_ns":elapsed.0.as_nanos(),"ns_per_write":elapsed.0.as_nanos() as f64/iterations as f64,
            "panic":if cfg!(panic="abort"){"abort"}else{"unwind"},
            "root_diagnostics":cfg!(feature="diagnostics"),"root_verification":cfg!(feature="verification"),
            "work":elapsed.1})
    }

    // Exact, retained symbols for Callgrind --toggle-collect. These delimit
    // runtime work only; setup, warmup, loop control and teardown are excluded.
    #[unsafe(no_mangle)]
    #[inline(never)]
    fn reactive_walk_set(engine: &Typed, input: <Typed as Engine>::Signal, value: u64) {
        engine.set(input, value);
    }
    #[unsafe(no_mangle)]
    #[inline(never)]
    fn reactive_walk_flush(engine: &Typed) {
        engine.settle();
    }
    #[unsafe(no_mangle)]
    #[inline(never)]
    fn reactive_sycamore_walk_set(
        engine: &Sycamore,
        input: <Sycamore as Engine>::Signal,
        value: u64,
    ) {
        engine.set(input, value);
    }
    #[unsafe(no_mangle)]
    #[inline(never)]
    fn reactive_sycamore_walk_flush(engine: &Sycamore) {
        engine.settle();
    }
    fn phase_probe<E: Engine + 'static>(
        shape: &str,
        size: usize,
        iterations: u64,
        set: fn(&E, E::Signal, u64),
        flush: fn(&E),
    ) -> serde_json::Value {
        if matches!(shape, "diamond" | "fan_out" | "conditional") {
            let fixture = match shape {
                "diamond" => Shape::Diamond,
                "fan_out" => Shape::FanOut,
                _ => Shape::Conditional,
            };
            let checked = Graph::<E>::new::<true>(fixture, size);
            checked.validate();
            drop(checked);
            let graph = Graph::<E>::new::<false>(fixture, size);
            for value in 1..=16 {
                graph.step(value);
            }
            for value in 17..iterations + 17 {
                set(&graph.engine, graph.input, black_box(value));
                flush(&graph.engine);
                black_box(graph.output.get());
            }
            assert_eq!(graph.output.get(), graph.expected(iterations + 16));
            return serde_json::json!({"phase_probe":true,"engine":E::NAME,"shape":shape,"size":size,"iterations":iterations,
                "panic":if cfg!(panic="abort"){"abort"}else{"unwind"},
                "root_diagnostics":cfg!(feature="diagnostics"),"root_verification":cfg!(feature="verification"),
                "work":graph.engine.work()});
        }
        assert!(matches!(shape, "chain" | "cut"));
        let cut = shape == "cut";
        let (checked, input, output, memos, effects) = chain_probe::<E, true>(size, cut);
        for value in 1..=4 {
            memos.set(0);
            effects.set(0);
            checked.set(input, value);
            checked.settle();
            assert_eq!(memos.get(), if cut { 1 } else { size });
            assert_eq!(effects.get(), usize::from(!cut));
            assert_eq!(output.get(), size as u64 + if cut { 0 } else { value });
        }
        drop(checked);
        let (engine, input, output, _, _) = chain_probe::<E, false>(size, cut);
        for value in 1..=16 {
            engine.set(input, value);
            engine.settle();
        }
        for value in 17..iterations + 17 {
            set(&engine, input, black_box(value));
            flush(&engine);
            black_box(output.get());
        }
        assert_eq!(
            output.get(),
            size as u64 + if cut { 0 } else { iterations + 16 }
        );
        serde_json::json!({"phase_probe":true,"engine":E::NAME,"shape":shape,"size":size,"iterations":iterations,
            "panic":if cfg!(panic="abort"){"abort"}else{"unwind"},
            "root_diagnostics":cfg!(feature="diagnostics"),"root_verification":cfg!(feature="verification"),
            "work":engine.work()})
    }

    pub(super) fn run() {
        let args: Vec<_> = std::env::args().skip(1).collect();
        assert!(
            args.len() == 4 || args.len() == 5,
            "usage: walk_diagnostic ENGINE SHAPE SIZE ITERATIONS [phases]"
        );
        let size = args[2].parse().unwrap();
        let iterations = args[3].parse().unwrap();
        assert!(size > 0 && iterations > 0);
        if args.len() == 5 {
            assert_eq!(args[4], "phases");
            let row = match args[0].as_str() {
                "typed" => phase_probe::<Typed>(
                    &args[1],
                    size,
                    iterations,
                    reactive_walk_set,
                    reactive_walk_flush,
                ),
                "sycamore" => phase_probe::<Sycamore>(
                    &args[1],
                    size,
                    iterations,
                    reactive_sycamore_walk_set,
                    reactive_sycamore_walk_flush,
                ),
                _ => panic!("phase probe supports typed and sycamore"),
            };
            println!("{row}");
            return;
        }
        let result = match args[0].as_str() {
            "typed" => probe::<Typed>(&args[1], size, iterations),
            "sycamore" => probe::<Sycamore>(&args[1], size, iterations),
            "production" => probe::<Local>(&args[1], size, iterations),
            _ => panic!("unknown engine"),
        };
        println!("{result}");
    }
}

fn main() {
    adapters::run();
}
