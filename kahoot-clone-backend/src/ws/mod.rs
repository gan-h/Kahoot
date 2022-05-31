/// Contains the schema of the websocket api.
///
/// All messages, both server -> client and client -> server, are in the form:
/// ```json
/// {
///     "type": "<message_type>",
///     "<field>": "<value>",
///     ...
/// }
/// ```
pub mod api;

/// Contains data for representing game states.
pub mod state;

use api::{Action, HostEvent, Question, RoomId, UserEvent};

use state::{GameEvent, PlayerAnswer, Room, SharedState, Users};

use crate::ext::{ToMessageExt, NextActionExt};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::WebSocket;
use axum::extract::WebSocketUpgrade;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};

use tokio::sync::{mpsc, watch};

use futures::{SinkExt, StreamExt};

use self::state::State;

/// Websocket api router.
pub fn router() -> Router {
    let rooms = Mutex::new(HashMap::new());
    let state = Arc::new(State { rooms });

    Router::new()
        // GET /
        .route("/", get(handle_ws_connection))
        // Includes the shared state in routes
        .layer(Extension(state))
}

/// Passes an upgraded websocket to `handle_socket`.
//
// Here, `WebSocketUpgrade` and `Extension` are "extractors" from the `axum`
// framework, and they allow `axum` to automatically detect how to parse web
// requests based on the type parameters of the function.
async fn handle_ws_connection(
    // Since this function has a `WebSocketUpgrade` paremeter, `axum` knows it
    // should accept websocket connections on this route.
    ws: WebSocketUpgrade,
    // This is an example of "destructuring"
    //
    // You can do something similar with tuples like
    // `let (a, b) = (1, 2);` which sets a = 1, and b = 2
    //
    // Relevant: https://doc.rust-lang.org/rust-by-example/flow_control/match/destructuring.html
    Extension(state): Extension<SharedState>,
) -> Response {
    ws.on_upgrade(|socket| handle_ws(socket, state))
}

/// Deals with an upgraded websocket.
async fn handle_ws(mut socket: WebSocket, state: SharedState) {
    let action = if let Some(action) = socket.next_action().await {
        action
    } else {
        eprintln!("Couldn't parse action");
        return;
    };

    match action {
        Action::CreateRoom { questions } => create_room(socket, state, questions).await,
        Action::JoinRoom { room_id, username } => join_room(socket, state, room_id, username).await,
        action => eprintln!("Invalid first action {action:?}"),
    };
}

/// Handles room creation.
///
/// The websocket will be treated as the "host" from now on.
async fn create_room(mut host: WebSocket, state: SharedState, questions: Vec<Question>) {
    eprintln!("Creating room...");

    let (action_tx, action_rx) = mpsc::channel(20);
    let (result_tx, result_rx) = watch::channel(GameEvent::InLobby);
    let (users, mut player_event_rx) = Users::new();

    // Create an empty room
    let room = Room {
        users,
        result_stream: result_rx,
        action_stream: action_tx,
    };

    // Put the room into an `Arc`
    let room = Arc::new(room);

    let room_id = state.insert_room(Arc::clone(&room));

    // Room creation event
    eprintln!("Sending room id: `{room_id}`");
    {
        let event = HostEvent::RoomCreated { room_id };
        let _ = host.send(event.to_message()).await;
    }

    let (mut host_tx, mut host_rx) = host.split();

    // Wrap the host transmitter with an `mpsc`
    let host_tx = {
        let (host_tx_mpsc, mut rx) = mpsc::channel::<HostEvent>(30);

        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if host_tx.send(event.to_message()).await.is_err() {
                    return;
                }
            }

            // Close connection
            let _ = host_tx.close().await;
        });

        host_tx_mpsc
    };

    // Forward player leave/join to host
    let join_leave_task = {
        let host_tx = host_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = player_event_rx.recv().await {
                let event = match event {
                    state::PlayerEvent::Joined(username) => HostEvent::UserJoined { username },
                    state::PlayerEvent::Left(username) => HostEvent::UserLeft { username },
                };

                let _ = host_tx.send(event).await;
            }
        })
    };

    // Wait until host begins room and there is at least one player in lobby
    loop {
        match host_rx.next_action().await {
            // Host tries to begin the first round
            Some(Action::BeginRound) => {
                eprintln!("Attempting to start game...");

                // Accquire lock on users mutex, and check the length
                if room.users.player_count() > 0 {
                    eprintln!("Starting game...");
                    break;
                } else {
                    eprintln!("Not enough players.");
                }
            }
            // Close room otherwise
            _ => {
                state.remove_room(&room_id).await;
                return;
            }
        }
    }

    let action_rx = Arc::new(tokio::sync::Mutex::new(action_rx));
    for question in questions.into_iter() {
        let point_gains = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Collect answers from users
        let mut answer_collect_task = {
            let host_tx = host_tx.clone();
            let action_rx = Arc::clone(&action_rx);
            let point_gains = Arc::clone(&point_gains);
            let correct_choice = question.answer;
            let room = Arc::clone(&room);

            tokio::spawn(async move {
                let mut answered = HashSet::new();
                let mut action_rx = action_rx.lock().await;
                let mut point_gains = point_gains.lock().await;
                let mut points = 1000;

                while let Some(PlayerAnswer { username, choice }) = action_rx.recv().await {
                    if answered.contains(&username) {
                        return;
                    }

                    answered.insert(username.clone());

                    // Tell host user answered
                    let _ = host_tx
                        .send(HostEvent::UserAnswered {
                            username: username.clone(),
                        })
                        .await;

                    eprintln!("`{username}` answered {choice}");

                    // If the choice is correct
                    if choice == correct_choice {
                        // Update points log
                        eprintln!("`{username}` +{points}");
                        point_gains.insert(username, points);

                        // Decrease next point gain
                        points = (points * 10 / 11).max(1);
                    }

                    // Has every player answered
                    let all_answered = room
                        .users
                        .users
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|name| answered.contains(name));

                    // If everyone has answered, finish task
                    if all_answered {
                        return;
                    }
                }
            })
        };

        // Save values
        let question_time = question.time as u64;
        let choice_count = question.choices.len();

        // Alert host that the round began
        eprintln!("Alerting host that round began...");
        let _ = host_tx.send(HostEvent::RoundBegin { question }).await;

        // Alert players a round began
        eprintln!("Alerting players that round began...");
        let _ = result_tx.send(GameEvent::RoundBegin { choice_count });

        // Wait for the time duration or for the task to fully complete
        let time_task = tokio::time::sleep(Duration::from_secs(question_time));
        tokio::pin!(time_task);
        tokio::select! {
            _ = (&mut time_task) => answer_collect_task.abort(),
            _ = (&mut answer_collect_task) => { drop(time_task) },
        };

        eprintln!("End of round...");

        let point_gains = point_gains.lock().await.clone();

        // Tell host that the round ended
        eprintln!("Alerting host that round ended...");
        let _ = host_tx
            .send(HostEvent::RoundEnd {
                point_gains: point_gains.clone(),
            })
            .await;

        // Alert players round ended
        eprintln!("Alerting players that round ended...");
        let _ = result_tx.send(GameEvent::RoundEnd {
            point_gains: Arc::new(point_gains),
        });

        // Wait until host begins next round
        match host_rx.next_action().await {
            Some(Action::BeginRound) => (),
            _ => {
                eprintln!("Closing room...");
                state.remove_room(&room_id).await;
                return;
            }
        }
    }

    eprintln!("Game is over!");

    // Alert host that the game ended
    eprintln!("Alerting host that game has ended...");
    let _ = host_tx.send(HostEvent::GameEnd).await;

    join_leave_task.abort();
    drop(room);

    state.remove_room(&room_id).await;

    // Alert players game ended
    let _ = result_tx.send(GameEvent::GameEnd);
}

/// Handles room joining.
///
/// The websocket will be treated as a "player" from now on.
async fn join_room(socket: WebSocket, state: SharedState, room_id: RoomId, username: String) {
    eprintln!("Finding room `{room_id}`...");
    let room = if let Some(room) = state.find_room(&room_id) {
        room
    } else {
        eprintln!("Couldn't find room `{room_id}`, disconnecting...");
        return;
    };

    eprintln!("Joining room...");

    let (mut user_tx, mut user_rx) = socket.split();
    let presence = if let Some(presence) = room.users.join_user(username.clone()).await {
        presence
    } else {
        eprintln!("User `{username}` already exists, disconnecting...");
        return;
    };

    // Watch for game status updates
    let mut game_event_task = {
        let mut event_watch = room.result_stream.clone();
        let username = username.clone();
        tokio::spawn(async move {
            // If the game status changed
            while let Ok(_) = event_watch.changed().await {
                let event = event_watch.borrow().clone();
                match event {
                    GameEvent::GameEnd => {
                        let event = UserEvent::GameEnd;
                        let _ = user_tx.send(event.to_message()).await;
                        
                        // Close connection
                        let _ = user_tx.close().await;
                    }
                    GameEvent::RoundBegin { choice_count } => {
                        let event = UserEvent::RoundBegin { choice_count };
                        let _ = user_tx.send(event.to_message()).await;
                    }
                    GameEvent::RoundEnd { point_gains } => {
                        let point_gain = point_gains.get(&username).copied();
                        let event = UserEvent::RoundEnd { point_gain };
                        let _ = user_tx.send(event.to_message()).await;
                    }
                    GameEvent::InLobby => (),
                }
            }
        })
    };

    // Feed user answers into action stream for the host to deal with
    let mut user_action_task = {
        let action_stream = room.action_stream.clone();
        tokio::spawn(async move {
            while let Some(action) = user_rx.next_action().await {
                if let Action::Answer { choice } = action {
                    let _ = action_stream
                        .send(PlayerAnswer {
                            username: username.clone(),
                            choice,
                        })
                        .await;
                }
            }
        })
    };

    // Wait until either task ends
    tokio::select! {
        _ = (&mut game_event_task) => user_action_task.abort(),
        _ = (&mut user_action_task) => game_event_task.abort(),
    };

    // Leaves room
    presence.leave().await;
}

/// Websocket api testing
#[cfg(test)]
mod tests {
    use crate::ws::router;
    use crate::ws::api::{Action, HostEvent, UserEvent, Question};

    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::{net::SocketAddr, time::Duration};
    use tokio::net::TcpStream;
    use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream, MaybeTlsStream};
    use futures::{StreamExt, SinkExt};
    use serde::Serialize;

    // `let_assert` is a useful testing macro asserting a specific enum variant
    // and destructuring the variant to get its inner value.
    use assert2::let_assert;

    use super::api::RoomId;

    static PORT: AtomicU16 = AtomicU16::new(3001);

    struct TestServer {
        port: u16,
    }

    type SocketStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

    impl TestServer {
        async fn new() -> Self {
            let port = PORT.fetch_add(1, Ordering::Relaxed);

            tokio::spawn(async move {
                axum::Server::bind(&SocketAddr::from(([127, 0, 0, 1], port)))
                    .serve(router().into_make_service())
                    .await
                    .unwrap();
            });

            // Wait a bit so server can start
            // TODO: Make this wait for the server to open, not for a specific amount of time
            tokio::time::sleep(Duration::from_secs(1)).await;

            Self { port }
        }

        async fn connect(&self) -> SocketStream {
            let (ws, _) = connect_async(format!("ws://127.0.0.1:{}", self.port))
                .await
                .unwrap();

            ws
        }

        async fn create_room(&self, questions: Vec<Question>) -> (SocketStream, RoomId) {
            let mut ws = self.connect().await;

            // Send create room action
            ws.send(serial(&Action::CreateRoom { questions })).await.unwrap();

            // Response must be a text message with no errors
            let_assert!(Some(Ok(Message::Text(s))) = ws.next().await);

            // Parse response
            let event: HostEvent = serde_json::from_str(&s).unwrap();

            // Response must be a room created event
            let_assert!(HostEvent::RoomCreated { room_id } = event);

            (ws, room_id)
        }

        async fn join_room(&self, room_id: RoomId, username: String) -> SocketStream {
            // Establish connection
            let mut ws = self.connect().await;

            // Send join room message
            ws.send(serial(&Action::JoinRoom {
                room_id,
                username,
            })).await.unwrap();

            ws
        }
    }

    // Macro magic, don't bother understanding
    macro_rules! question {
        ($ques:expr , time: $time:expr => [ $($correct:expr => $choice:expr),+ $(,)? ]) => {
            {
                let mut answer = 0;
                let mut answer_count = 0;
                let mut choices = Vec::new();

                $(
                    choices.push(String::from($choice));
                    if $correct {
                        answer_count += 1;
                    } else {
                        answer += 1;
                    }
                )+

                assert!(answer_count == 1, "Must have one correct answer");

                Question {
                    question: String::from($ques),
                    time: $time,
                    choices,
                    answer,
                }
            }
        };
    }

    /// Tests a simple situation where there is one player and only one question.
    #[tokio::test]
    async fn one_player_and_question() {
        // Start server
        let server = TestServer::new().await;

        // Sample question
        let question = question! {
            "Fish?", time: 30 => [
                true => "foo",
                false => "bar",
            ]
        };

        // Start room
        let (host_ws, room_id) = server.create_room(vec![question.clone()]).await;
        let (mut host_tx, mut host_rx) = host_ws.split();

        // Host tests
        let question_clone = question.clone();
        let host_task = tokio::spawn(async move {
            // User joined event
            let_assert!(Some(Ok(Message::Text(s))) = host_rx.next().await);
            let event: HostEvent = serde_json::from_str(&s).unwrap();
            let_assert!(HostEvent::UserJoined { username } = event);

            // Username matches
            assert_eq!("Johnny", &username);

            // Send begin round action
            host_tx.send(serial(&Action::BeginRound)).await.unwrap();

            // Round begin event
            let_assert!(Some(Ok(Message::Text(s))) = host_rx.next().await);
            let event: HostEvent = serde_json::from_str(&s).unwrap();
            let_assert!(HostEvent::RoundBegin { question } = event);

            // Check if the question is the same
            assert_eq!(question_clone, question);

            // User answered event
            let_assert!(Some(Ok(Message::Text(s))) = host_rx.next().await);
            let event: HostEvent = serde_json::from_str(&s).unwrap();
            let_assert!(HostEvent::UserAnswered { username } = event);

            // Username matches
            assert_eq!("Johnny", &username);

            // Round end event
            let_assert!(Some(Ok(Message::Text(s))) = host_rx.next().await);
            let event: HostEvent = serde_json::from_str(&s).unwrap();
            let_assert!(HostEvent::RoundEnd { point_gains } = event);

            // Johnny gained 1000 points
            assert_eq!(point_gains.get("Johnny"), Some(&1000));

            // Send begin round action
            host_tx.send(serial(&Action::BeginRound)).await.unwrap();

            // Game end event
            let_assert!(Some(Ok(Message::Text(s))) = host_rx.next().await);
            let event: HostEvent = serde_json::from_str(&s).unwrap();
            let_assert!(HostEvent::GameEnd = event);
        });

        // Player tests
        let user_ws = server.join_room(room_id, String::from("Johnny")).await;
        let user_task = tokio::spawn(async move {
            let (mut user_tx, mut user_rx) = user_ws.split();

            // Round begin event
            let_assert!(Some(Ok(Message::Text(s))) = user_rx.next().await);
            let event: UserEvent = serde_json::from_str(&s).unwrap();
            let_assert!(UserEvent::RoundBegin { choice_count } = event);

            // Has correct choice count
            assert_eq!(question.choices.len(), choice_count);

            // Send correct answer
            user_tx.send(serial(&Action::Answer {
                choice: question.answer,
            })).await.unwrap();

            // Round end event
            let_assert!(Some(Ok(Message::Text(s))) = user_rx.next().await);
            let event: UserEvent = serde_json::from_str(&s).unwrap();
            let_assert!(UserEvent::RoundEnd { point_gain: Some(point_gain) } = event);

            // Gained 1000 points
            assert_eq!(point_gain, 1000);

            // Game end event
            let_assert!(Some(Ok(Message::Text(s))) = user_rx.next().await);
            let event: UserEvent = serde_json::from_str(&s).unwrap();
            let_assert!(UserEvent::GameEnd = event);
        });

        // Wait for both tasks to complete
        tokio::try_join!(host_task, user_task).unwrap();
    }

    #[tokio::test]
    async fn join_leave() {
        let server = TestServer::new().await;

        let (mut host_ws, room_id) = server.create_room(vec![
            question! {
                "Fish?", time: 30 => [
                    true => "foo",
                    false => "bar",
                ]
            }
        ]).await;

        // Host tests
        let host_task = tokio::spawn(async move {
            let mut joined = HashSet::new();
            let mut left = HashSet::new();

            let mut i = 0;
            while let Some(Ok(Message::Text(s))) = host_ws.next().await {
                let event: HostEvent = serde_json::from_str(&s).unwrap();
                match event {
                    HostEvent::UserJoined { username } => {
                        assert!(joined.insert(username.clone()), "{username} joined twice");
                    }
                    HostEvent::UserLeft { username } => {
                        assert!(joined.contains(&username), "{username} left before joining");
                        assert!(left.insert(username.clone()), "{username} left twice");
                    }
                    _ => panic!("Unexpected event {event:?}"),
                }

                i += 1;
                if i >= 6 {
                    break;
                }
            }

            let names = HashSet::from([
                String::from("Alice"),
                String::from("Bob"),
                String::from("Chris"),
            ]);
            assert_eq!(joined, names);
            assert_eq!(left, names);
        });

        // Alice join
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut alice = server.join_room(room_id, String::from("Alice")).await;

        // Bob join
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut bob = server.join_room(room_id, String::from("Bob")).await;

        // Alice leave
        tokio::time::sleep(Duration::from_millis(200)).await;
        alice.close(None).await.unwrap();

        // Chris join
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut chris = server.join_room(room_id, String::from("Chris")).await;

        // Chris leave
        tokio::time::sleep(Duration::from_millis(200)).await;
        chris.close(None).await.unwrap();

        // Bob leave
        tokio::time::sleep(Duration::from_millis(200)).await;
        bob.close(None).await.unwrap();

        host_task.await.unwrap();
    }

    /// Convert a `Serialize`able into a JSON message.
    fn serial(s: &impl Serialize) -> Message {
        let json_string = serde_json::to_string(s).unwrap();
        Message::text(json_string)
    }
}