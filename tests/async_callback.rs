use piccolo::{meta_ops, AsyncSequence, Closure, Executor, Lua, StaticError, Table, Variadic};

#[test]
fn async_sequence() -> Result<(), StaticError> {
    let mut lua = Lua::core();

    lua.try_enter(|ctx| {
        let callback = AsyncSequence::new_callback(&ctx, |mut seq| {
            Box::new(async move {
                let (table, length) = seq.try_enter(|ctx, locals, _, mut stack| {
                    let table: Table = stack.consume(ctx)?;
                    let length = table.length();
                    Ok((locals.stash(&ctx, table), length))
                })?;

                for i in 1..=length {
                    let function = seq.try_enter(|ctx, locals, _, _| {
                        let table = locals.fetch(&table);
                        let func = meta_ops::call(ctx, table.get(ctx, i))?;
                        Ok(locals.stash(&ctx, func))
                    })?;

                    seq.call(&function, 0).await?
                }

                Ok(seq.return_to())
            })
        });
        ctx.set_global("callback", callback)?;
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            None,
            &br#"
                return callback({
                    function() return 1, 2, 3 end,
                    function(...) return 4, 5, ... end,
                    function(...) return 6, 7, ... end,
                })
            "#[..],
        )?;

        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    let v = lua.execute::<Variadic<Vec<i64>>>(&executor)?;

    assert_eq!(&v.0, &[6, 7, 4, 5, 1, 2, 3]);

    Ok(())
}
