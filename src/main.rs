use reactive::Runtime;

fn main() -> Result<(), reactive::RuntimeError> {
    let runtime = Runtime::new();
    let root = runtime.create_root()?;

    let count = runtime.with_owner(root, |rt| rt.signal(1))?;

    let doubled = runtime.with_owner(root, |rt| {
        rt.memo(move |rt| Ok(count.try_get_copy(rt)? * 2))
    })?;

    runtime.with_owner(root, |rt| {
        rt.effect(move |rt| {
            println!("Dobro: {}", doubled.try_get_copy(rt)?);
            Ok(())
        })
    })?;

    runtime.flush(Default::default())?; // Dobro: 2

    count.try_set(&runtime, 3)?;
    runtime.flush(Default::default())?; // Dobro: 6

    runtime.shutdown()
}
