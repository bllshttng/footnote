//! The attached client's read loop: frame intake, and the refusal that
//! keeps the connection alive when one frame cannot be decoded.

use super::*;
use tokio::net::unix::OwnedReadHalf;

/// Reliable inbound path: every message is awaited into the core channel.
/// Any read error (including an abruptly killed client) deregisters the
/// client and leaves every pane untouched (AC4-HP).
pub(super) async fn client_reader(mut r: OwnedReadHalf, core_tx: mpsc::Sender<CoreMsg>, id: u64) {
    loop {
        match read_msg::<_, ClientMsg>(&mut r).await {
            Ok(ClientMsg::Input(bytes)) => {
                if core_tx.send(CoreMsg::Input { id, bytes }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Resize { rows, cols }) => {
                if core_tx
                    .send(CoreMsg::Resize { id, rows, cols })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::Command(cmd)) => {
                if core_tx.send(CoreMsg::Command { id, cmd }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Mouse { pane, event }) => {
                if core_tx
                    .send(CoreMsg::Mouse { id, pane, event })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::LinkHover {
                pane,
                row,
                col,
                seq,
            }) => {
                if core_tx
                    .send(CoreMsg::LinkHover {
                        id,
                        pane,
                        row,
                        col,
                        seq,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::BlockJump { pane, dir }) => {
                if core_tx
                    .send(CoreMsg::BlockNav {
                        id,
                        pane,
                        op: BlockNavOp::Jump(dir),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::BlockSelect { pane, dir }) => {
                if core_tx
                    .send(CoreMsg::BlockNav {
                        id,
                        pane,
                        op: BlockNavOp::Select(dir),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::BlockRerun { pane }) => {
                if core_tx
                    .send(CoreMsg::BlockNav {
                        id,
                        pane,
                        op: BlockNavOp::Rerun,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::SearchOpen { pane, query }) => {
                if core_tx
                    .send(CoreMsg::Search {
                        id,
                        pane,
                        op: SearchOp::Open(query),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::SearchStep { pane, dir }) => {
                if core_tx
                    .send(CoreMsg::Search {
                        id,
                        pane,
                        op: SearchOp::Step(dir),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::SearchClear { pane }) => {
                if core_tx
                    .send(CoreMsg::Search {
                        id,
                        pane,
                        op: SearchOp::Clear,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::PaneAnswer {
                pane,
                fingerprint,
                region_lines,
                keystroke,
            }) => {
                if core_tx
                    .send(CoreMsg::PaneAnswer {
                        id,
                        pane,
                        fingerprint,
                        region_lines,
                        keystroke,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::DispatchNext { account }) => {
                if core_tx
                    .send(CoreMsg::DispatchNext { id, account })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::AgentLaunch(request)) => {
                if core_tx
                    .send(CoreMsg::AgentLaunch { id, request })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(ClientMsg::Detach) => {
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
            // A second Attach, a pre-Attach-only Query/KillServer, or a
            // one-shot Control on a live connection is a protocol violation:
            // log it (this stderr is the session log) and close rather than
            // acting on a confused stream.
            Ok(
                msg @ (ClientMsg::Attach { .. }
                | ClientMsg::Query
                | ClientMsg::KillServer
                | ClientMsg::Control { .. }),
            ) => {
                let name = match msg {
                    ClientMsg::Attach { .. } => "Attach",
                    ClientMsg::Query => "Query",
                    ClientMsg::Control { .. } => "Control",
                    _ => "KillServer",
                };
                eprintln!("fno mux: client {id} sent {name} on a live connection; dropping it");
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
            // A frame whose JSON body fails to decode (an unknown Command
            // variant above all: protocol skew) is refused, not fatal. The
            // length prefix and body were fully consumed before the decode,
            // so the stream is still on a frame boundary - the client loses
            // a notice, never the session. Every other error (io, an
            // over-cap prefix whose body was never read, a closed peer)
            // desyncs or has nothing left to read: the client is gone.
            Err(crate::proto::ProtoError::Malformed(e)) => {
                eprintln!("fno mux: client {id} sent an unusable frame: {e}");
                let _ = core_tx
                    .send(CoreMsg::FrameRefused {
                        id,
                        reason: format!(
                            "unusable message refused; server speaks wire v{} ({e})",
                            crate::proto::PROTO_VERSION
                        ),
                    })
                    .await;
            }
            Err(e) => {
                // Includes the abrupt-close case (killed client): routine, but
                // one log line makes a misbehaving client diagnosable.
                if !matches!(e, crate::proto::ProtoError::Closed) {
                    eprintln!("fno mux: client {id} read failed: {e}");
                }
                let _ = core_tx.send(CoreMsg::Gone(id)).await;
                break;
            }
        }
    }
}
