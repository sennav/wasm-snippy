use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Router, http::StatusCode, Json,
};
use std::{net::SocketAddr, path::PathBuf};
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, TraceLayer},
};

use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

//allows to extract the IP of connecting user
use axum::extract::connect_info::ConnectInfo;

//allows to split the websocket stream into separate TX and RX branches
use futures::{sink::SinkExt, stream::StreamExt};

use tournament::{BotDetails, BotRunType, Tournament};

use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::NoTls;

use std::env;
use dotenvy::dotenv;

mod tournament;

pub type ConnectionPool = Pool<PostgresConnectionManager<NoTls>>;

struct SharedState {
    tournament: Mutex<Option<Tournament>>,
    broadcast_channel: broadcast::Sender<String>,
    db_pool: ConnectionPool,
}

#[tokio::main]
async fn main() {
    // Load .env file
    dotenv().ok();

    // initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wasi-runner=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let assets_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

    // Set up DB connection pool
    let config: tokio_postgres::Config = env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set")
        .parse()
        .expect("DATABASE_URL must be a valid postgresql connection string");
    let manager = PostgresConnectionManager::new(config, NoTls);
    let db_pool = Pool::builder().build(manager).await.unwrap();

    let (tx, _rx) = broadcast::channel(100);
    let shared_state: Arc<SharedState> = Arc::new(SharedState {
        tournament: Mutex::new(None),
        broadcast_channel: tx,
        db_pool
    });

    // Can call init to pre-warm the wasm runtime but it slows down startup.
    tournament::init();

    // build our application with a route
    let app = Router::new()
        .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        // `GET /` goes to `root`
        .route("/", get(root))
        .route("/api/ws", get(ws_handler))
        .route("/health", get(health))
        .route("/api/test", post(test_bot))
        .route("/api/bot", post(post_bot))
        .route("/api/tournament", post(create_tournament))
        .with_state(shared_state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
    tracing::debug!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

async fn root() -> &'static str {
    "Hello, World!"
}

async fn health() -> &'static str {
    "Ok"
}

#[derive(Deserialize)]
struct TestBotRequest {
    botcode: String,
    run_type: BotRunType,
}

#[derive(Deserialize)]
struct CreateBotRequest {
    name: String,
    botcode: String,
    run_type: BotRunType,
}

async fn post_bot(State(shared_state): State<Arc<SharedState>>, Json(payload): Json<CreateBotRequest>) -> Response {
    let botname = payload.name;
    let botcode = payload.botcode;

    let bot: BotDetails = BotDetails {
        id: None,
        run_type: payload.run_type,
        name: botname.clone(),
        code: botcode.clone(),
        wasm_path: "".to_string(),
    };

    if botname.len() > 30 {
        return (StatusCode::BAD_REQUEST, Json("Bot name is limited to 30 characters.")).into_response();
    }
    if botname.len() == 0 {
        return (StatusCode::BAD_REQUEST, Json("Bot name cannot be empty.")).into_response();
    }

    let result = tournament::add_bot(&shared_state.db_pool, &bot).await;
    return match result {
        Ok(1) => (StatusCode::OK, Json("success!")).into_response(),
        Ok(_) => (StatusCode::BAD_REQUEST, Json("Bot name is already in use.")).into_response(),
        Err(e) => {
            println!("Error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json("Unexpected error occurred".to_string())).into_response();
        }
    }
}

async fn create_tournament(State(shared_state): State<Arc<SharedState>>) -> Response {
    let result = tournament::create_tournament(&shared_state.db_pool).await;
    match result {
        Ok(mut payload) => {
            let mut tournament: std::sync::MutexGuard<'_, Option<Tournament>> = shared_state.tournament.lock().unwrap();
            *tournament = Some(payload.clone());
            let tournament_json = serde_json::to_string(&payload).unwrap();
            shared_state.broadcast_channel.send(tournament_json).unwrap();
            let result2 = payload.run(&shared_state.broadcast_channel);            
            *tournament = Some(payload.clone());
            match result2 {
                Ok(_) => (StatusCode::OK).into_response(),
                Err(e) => {
                    println!("Error: {}", e);
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json("Unexpected error occurred".to_string())).into_response();
                }
            }
        }
        Err(e) => {
            println!("Error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json("Unexpected error occurred".to_string())).into_response();
        }
    }
}

async fn test_bot(Json(payload): Json<TestBotRequest>) -> Response {
    let botcode = payload.botcode;

    let bot: BotDetails = BotDetails {
        id: None,
        run_type: payload.run_type,
        name: "test".to_string(),
        code: botcode.clone(),
        wasm_path: "".to_string(),
    };

    let result = tournament::test_bot(&bot);
    match result {
        Ok(payload) => (StatusCode::OK, Json(payload)).into_response(),
        Err(e) => {
            println!("Error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json("Unexpected error occurred".to_string())).into_response();
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(shared_state): State<Arc<SharedState>>,
) -> impl IntoResponse {
    let user_agent = String::from("Unknown browser");
    println!("`{user_agent}` at {addr} connected.");
    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, addr, shared_state))
}

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(socket: WebSocket, who: SocketAddr, state: Arc<SharedState>) {
    // By splitting socket we can send and receive at the same time. In this example we will send
    // unsolicited messages to client based on some sort of server's internal event (i.e .timer).
    let (mut sender, mut receiver) = socket.split();

    // Start recieveing updates
    let mut update_reciever = state.broadcast_channel.subscribe();
    
    let tournament_json = match serde_json::to_string(&state.tournament) {
        Ok(json) => json,
        Err(e) => {
            println!("Error: {}", e);
            return;
        }
    };

    // Send the tournament state to the client
    if sender.send(Message::Text(tournament_json)).await.is_err() {
        return;
    }

    let mut send_task = tokio::spawn(async move {
        // Send updates to the client too.
        while let Ok(msg) = update_reciever.recv().await {
            println!("Sending update to client {}", who);
            // In any websocket error, break loop.
            if sender.send(Message::Text(msg)).await.is_err() {
                println!("Error for client {}, breaking.", who);
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(message) = receiver.next().await {
            match message {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(_)) => {
                    println!("Received close message from {}", who);
                    break;
                }
                Err(e) => {
                    println!("Error: {}", e);
                    break;
                },
                _ => continue,
            };
        }
    });
   
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };
    println!("Websocket context {} destroyed", who);
}
