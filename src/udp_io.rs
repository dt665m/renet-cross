use std::{io, num::NonZeroUsize};

/// Run bounded receive attempts, not just successful/valid packets. Keeping the
/// IO operation injectable makes scheduling regressions independent of OS timing.
pub(crate) fn receive_with_budget(
    limit: NonZeroUsize,
    mut receive: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    for _ in 0..limit.get() {
        match receive() {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionReset
                ) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_socket_and_repeated_errors_cannot_exceed_receive_budget() {
        for outcome in [
            None,
            Some(io::ErrorKind::Interrupted),
            Some(io::ErrorKind::ConnectionReset),
        ] {
            let mut attempts = 0;
            receive_with_budget(NonZeroUsize::new(3).unwrap(), || {
                attempts += 1;
                outcome.map_or(Ok(()), |kind| Err(io::Error::from(kind)))
            })
            .unwrap();
            assert_eq!(attempts, 3);
        }
    }

    #[test]
    fn empty_socket_yields_and_fatal_error_is_preserved() {
        for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::PermissionDenied] {
            let mut attempts = 0;
            let result = receive_with_budget(NonZeroUsize::new(3).unwrap(), || {
                attempts += 1;
                Err(io::Error::from(kind))
            });
            assert_eq!(attempts, 1);
            if kind == io::ErrorKind::WouldBlock {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().kind(), kind);
            }
        }
    }
}
