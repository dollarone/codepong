use std::{
	convert::TryInto,
	fmt::Debug,
	path::{Path, PathBuf},
	sync::Arc,
	time::{Duration, Instant},
};

use anyhow::{
	Context,
	anyhow,
	bail,
};
use git2::Repository;
use handlebars::Handlebars;
use hyper::{
	Body,
	Method,
	Response,
	Server,
	StatusCode,
	service::{make_service_fn, service_fn},
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

type Request = hyper::Request <hyper::Body>;
type ResponseB = hyper::Response <hyper::Body>;
type ResultResponse = anyhow::Result <ResponseB>;

const REPO_PATH: &'static str = "game/git/repo";
const IRC_CONFIG_PATH: &'static str = "game/irc.toml";

#[tokio::main]
async fn main () -> anyhow::Result <()> {
	use std::{
		convert::Infallible,
		sync::Arc,
	};
	
	tracing_subscriber::fmt::init ();
	
	let baton = Baton::load ().await?;
	let first_timeout = baton.get ().map (|hold| hold.expiration);
	let baton = Arc::new (Mutex::new (baton));
	
	let irc_config = irc::client::prelude::Config::load (IRC_CONFIG_PATH)?;
	let irc_client = irc::client::prelude::Client::from_config (irc_config.clone ()).await?;
	irc_client.identify ()?;
	let irc_sender = irc_client.sender ();
	
	let baton_2 = Arc::clone (&baton);
	tokio::spawn (async move {
		let mut bot = IrcBot {
			client: irc_client,
			baton: baton_2,
		};
		bot.run ().await
	});
	
	let (timeout_tx, mut timeout_rx) = tokio::sync::watch::channel (first_timeout
	.map (|first_timeout| {
		let epoch_now = chrono::Utc::now ().timestamp ();
		let remaining = first_timeout - epoch_now;
		Instant::now () + Duration::from_secs (remaining.try_into ().unwrap ())
	}));
	
	let code_pong_server = Arc::new (CodePongServer {
		handlebars: Default::default (),
		irc_sender,
		irc_config,
		baton,
		timeout_tx,
	});
	
	let server_2 = Arc::clone (&code_pong_server);
	tokio::spawn (async move {
		let x = *timeout_rx.borrow ();
		if let Some (x) = x {
			tokio::time::sleep_until (x.into ()).await;
			if *timeout_rx.borrow () == Some (x) {
				server_2.send_irc_notification ("Baton timed out.").ok ();
			}
		}
		
		while timeout_rx.changed ().await.is_ok () {
			let x = *timeout_rx.borrow ();
			if let Some (x) = x {
				tokio::time::sleep_until (x.into ()).await;
				if *timeout_rx.borrow () == Some (x) {
					server_2.send_irc_notification ("Baton timed out.").ok ();
				}
			}
		}
	});
	
	let make_svc = make_service_fn (|_conn| {
		let code_pong_server = Arc::clone (&code_pong_server);
		
		async {
			let f = service_fn (move |req| {
				let code_pong_server = Arc::clone (&code_pong_server);
				
				async move {
					let r = code_pong_server.handle_all (req).await;
					let r = match r {
						Ok (r) => r,
						Err (e) => {
							tracing::error! ("{:?}", e);
							Response::builder ()
							.status (StatusCode::INTERNAL_SERVER_ERROR)
							.body (Body::from (format! ("{:?}", e).as_bytes ().to_vec ())).unwrap ()
						}
					};
					
					Ok::<_, Infallible> (r)
				}
			});
			
			Ok::<_, Infallible> (f)
		}
	});
	
	let server = Server::bind (&std::net::SocketAddr::from (([0, 0, 0, 0], 4000)))
	.serve (make_svc);
	
	server.await?;
	
	Ok (())
}

#[derive (Deserialize, Serialize)]
struct BatonHold {
	username: String,
	expiration: i64,
}

#[derive (Default, Deserialize, Serialize)]
struct Baton {
	hold: Option <BatonHold>,
}

const BATON_FILE: &'static str = "game/baton.json";

impl Baton {
	fn status (&self) -> String {
		let hold = self.get ();
		
		match hold {
			None => "The baton is free as in speech.".to_string (),
			Some (hold) => format! ("{} holds the baton.", hold.username),
		}
	}
	
	fn get (&self) -> Option <&BatonHold> {
		let hold = match &self.hold {
			None => return None,
			Some (x) => x,
		};
		
		let now = chrono::Utc::now ().timestamp ();
		
		if now >= hold.expiration {
			return None;
		}
		
		Some (hold)
	}
	
	async fn load () -> anyhow::Result <Self> {
		let s = match tokio::fs::read_to_string (BATON_FILE).await {
			Err (_) => return Ok (Self::default ()),
			Ok (x) => x,
		};
		
		let b: Baton = serde_json::from_str (&s)?;
		
		Ok (b)
	}
	
	async fn save (&self) -> anyhow::Result <()> {
		let s = serde_json::to_string (&self)?;
		
		let temp_path = format! ("{}.temp", BATON_FILE);
		tokio::fs::write (Path::new (&temp_path), s).await?;
		tokio::fs::rename (temp_path, BATON_FILE).await?;
		
		Ok (())
	}
	
	async fn next (&mut self, username: String, hold_seconds: i64) -> anyhow::Result <bool> {
		if self.get ().is_some () {
			Ok (false)
		}
		else {
			self.hold = Some (BatonHold {
				username,
				expiration: chrono::Utc::now ().timestamp () + hold_seconds,
			});
			self.save ().await?;
			Ok (true)
		}
	}
	
	fn can_commit (&self, username: &str) -> bool {
		match &self.hold {
			None => true,
			Some (hold) => hold.username == username,
		}
	}
	
	async fn commit (&mut self, username: &str) -> anyhow::Result <bool> {
		if ! self.can_commit (username) {
			return Ok (false);
		}
		
		self.hold = None;
		self.save ().await?;
		Ok (true)
	}
}

struct CodePongServer <'a> {
	handlebars: Handlebars <'a>,
	irc_sender: irc::client::Sender,
	irc_config: irc::client::prelude::Config,
	baton: Arc <Mutex <Baton>>,
	timeout_tx: tokio::sync::watch::Sender <Option <Instant>>,
}

#[derive (Serialize)]
struct CommitPage {
	commit_id: String,
	entries: Vec <EntryData>,
}

#[derive (Serialize)]
struct EntryData {
	name: Option <String>,
}

struct IrcBot {
	client: irc::client::prelude::Client,
	baton: Arc <Mutex <Baton>>,
}

impl IrcBot {
	async fn run (&mut self) -> anyhow::Result <()> {
		use irc::client::prelude::*;
		use futures::prelude::*;
		
		let mut stream = self.client.stream ()?;
		
		while let Some (message) = stream.next ().await.transpose ()? {
			let (channel, message) = match message.command {
				Command::PRIVMSG (c, m) => (c, m),
				_ => continue,
			};
			
			match self.handle_privmsg (&channel, &message).await {
				Err (e) => tracing::error! ("{:?}", e),
				Ok (_) => (),
			}
		}
		
		Ok (())
	}
	
	async fn handle_privmsg (&self, channel: &str, message: &str) 
	-> anyhow::Result <()> 
	{
		use BotCommand::*;
		
		let cmd = match parse_irc_privmsg (self.client.current_nickname (), &message) {
			Some (x) => x,
			None => return Ok (()),
		};
		
		let reply = match cmd {
			Help => "Commands: help, status, head\r\nhttps://six-five-six-four.com/codepong/".to_string (),
			GetStatus => self.handle_status ().await?,
			GetLastCommit => Self::handle_head ()?,
		};
		
		self.client.send_privmsg (&channel, reply).unwrap ();
		
		Ok (())
	}
	
	fn handle_head () -> anyhow::Result <String>
	{
		let commits = get_last_commits (1)?;
		let commit = match commits.get (0) {
			None => return Ok ("No commits yet".to_string ()),
			Some (x) => x,
		};
		
		Ok (commit.message.clone ().unwrap_or_else (|| "<no message>".to_string ()))
	}
	
	async fn handle_status (&self) -> anyhow::Result <String>
	{
		let baton = self.baton.lock ().await;
		Ok (baton.status ())
	}
}

impl CodePongServer <'_> {
	#[tracing::instrument (level = "debug", skip (self, req))]
	async fn handle_all (&self, req: Request) -> ResultResponse
	{
		use std::future::Future;
		
		async fn get_only <F, H> (req: Request, f: H) -> ResultResponse 
		where
		F: Send + Future <Output = ResultResponse>,
		H: Send + FnOnce (Request) -> F
		{
			match req.method () {
				&Method::GET => f (req).await,
				_ => method_not_allowed (),
			}
		}
		
		fn method_not_allowed () -> ResultResponse { 
			Ok (handle_error (StatusCode::METHOD_NOT_ALLOWED, "405 Method Not Allowed")?)
		}
		
		let uri = req.uri ().path ();
		tracing::debug! ("URI: {}", uri);
		
		if let Some (tail) = uri.strip_prefix ("/static/") {
			match req.method () {
				&Method::GET => self.handle_static (tail).await,
				_ => method_not_allowed (),
			}
		}
		else if uri == "/" {
			Ok (Response::builder ()
			.status (StatusCode::TEMPORARY_REDIRECT)
			.header ("location", "home")
			.body (Body::from ("Redirecting to home..."))?)
		}
		else if uri == "/home" {
			get_only (req, |_| self.handle_index ()).await
		}
		else if uri == "/next" {
			match req.method () {
				&Method::GET => self.handle_next_get ().await,
				&Method::POST => self.handle_next_post (req).await,
				_ => method_not_allowed (),
			}
		}
		else if uri == "/commit" {
			match req.method () {
				&Method::GET => self.handle_commit_get ().await,
				&Method::POST => self.handle_commit_post (req).await,
				_ => method_not_allowed (),
			}
		}
		else if uri == "/debug" {
			// self.send_irc_notification ("Someone clicked something!")?;
			
			Ok (Response::builder ()
			.body (Body::from ("Ok"))?)
		}
		else if let Some (tail) = uri.strip_prefix ("/git/") {
			match req.method () {
				&Method::GET => self.handle_git (tail).await,
				_ => method_not_allowed (),
			}
		}
		else if let Some (tail) = uri.strip_prefix ("/tree/") {
			match req.method () {
				&Method::GET => self.handle_tree (tail).await,
				_ => method_not_allowed (),
			}
		}
		else {
			Ok (handle_error (StatusCode::NOT_FOUND, "404 Not Found")?)
		}
	}
	
	fn send_irc_notification (&self, msg: &str) -> anyhow::Result <()>
	{
		for channel in &self.irc_config.channels {
			self.irc_sender.send_privmsg (channel, msg)?;
		}
		
		Ok (())
	}
	
	async fn handle_index (&self) -> anyhow::Result <ResponseB>
	{
		#[derive (Serialize)]
		struct Page <'a> {
			commits: Vec <CommitDisplay <'a>>,
			commit_count: usize,
			kb_free: u64,
			baton_status: String,
		}
		
		#[derive (Serialize)]
		struct CommitDisplay <'a> {
			id: &'a str,
			id_short: &'a str,
			author: Option <&'a str>,
			time: &'a str,
			message: Option <&'a str>,
		}
		
		let commits = get_last_commits (20)?;
		
		// I hate UOM and I hate heim too
		let kb_free = heim::disk::usage (REPO_PATH).await?
		.free ().get::<uom::si::information::kibibyte> ();
		
		let baton_status = {
			let baton = self.baton.lock ().await;
			baton.status ()
		};
		
		let page = Page {
			commits: commits.iter ()
			.map (|c| CommitDisplay {
				id: &c.id,
				id_short: &c.id [0..8],
				author: c.author.as_deref (),
				time: &c.time,
				message: c.message.as_deref (),
			})
			.collect (),
			commit_count: commits.len (),
			kb_free,
			baton_status,
		};
		
		self.template_response ("handlebars/index.hbs", &page).await
	}
	
	async fn handle_next_post (&self, req: Request) -> ResultResponse 
	{
		#[derive (Deserialize)]
		struct PostData {
			username: String,
		}
		
		let (_parts, body) = req.into_parts ();
		let form_data = read_body_limited (body, 1_024).await?;
		let data: PostData = serde_urlencoded::from_bytes (&form_data)?;
		
		let hold_seconds: u32 = 3_600;
		
		{
			let mut baton = self.baton.lock ().await;
			if ! baton.next (data.username.clone (), hold_seconds.into ()).await? {
				bail! ("Someone (maybe you) already has the baton.");
			}
		}
		
		self.send_irc_notification (&format! ("The baton was taken by {}", data.username))?;
		self.timeout_tx.send (Some (Instant::now () + Duration::from_secs (hold_seconds.into ())))?;
		
		Ok (Response::builder ()
		.status (StatusCode::SEE_OTHER)
		.header ("location", "home")
		.body (Body::from ("Took the baton!"))?)
	}
	
	async fn handle_commit_post (&self, req: Request) -> ResultResponse
	{
		#[derive (Deserialize)]
		struct PostData {
			username: String,
			url: String,
		}
		
		let (_parts, body) = req.into_parts ();
		let form_data = read_body_limited (body, 1_024).await?;
		let data: PostData = serde_urlencoded::from_bytes (&form_data)?;
		
		if ! data.url.starts_with ("https://") && ! data.url.starts_with ("ssh://")
		{
			bail! ("Git remote URL must use HTTPS or SSH protocol");
		}
		
		let repo = Repository::open (REPO_PATH)?;
		
		{
			let mut baton = self.baton.lock ().await;
			if ! baton.can_commit (&data.username) {
				bail! ("You can't commit now. Someone else has the baton.");
			}
			
			{
				let mut remote = repo.remote_anonymous (&data.url)?;
				remote.fetch (&["main"], None, None)?;
				
				let fetch_head_obj = repo.revparse_single ("FETCH_HEAD")?;
				let fetch_head = repo.annotated_commit_from_fetchhead ("main", &data.url, &fetch_head_obj.id ())?;
				let (analysis, _) = repo.merge_analysis (&[&fetch_head])?;
				
				if analysis.is_up_to_date () {
					bail! ("Already up-to-date with `main` on that remote");
				}
				if ! analysis.is_fast_forward () {
					bail! ("Cannot fast-forward merge to Git remote's `main` branch");
				}
				
				repo.reset (&fetch_head_obj, git2::ResetType::Hard, None)?;
			}
			
			baton.commit (&data.username).await?;
		}
		
		self.timeout_tx.send (None)?;
		self.send_irc_notification (&format! ("A commit was made by {}", data.username))?;
		
		let msg = format! ("Fast-forwarded to {}!", data.url);
		
		Ok (Response::builder ()
		.status (StatusCode::SEE_OTHER)
		.header ("location", "home")
		.body (Body::from (msg))?)
	}
	
	async fn handle_next_get (&self) -> ResultResponse
	{
		#[derive (Serialize)]
		struct Page {
			
		}
		
		let page = Page {
			
		};
		
		self.template_response ("handlebars/next.hbs", &page).await
	}
	
	async fn handle_commit_get (&self) -> ResultResponse
	{
		#[derive (Serialize)]
		struct Page {
			holding_username: Option <String>,
		}
		
		let holding_username = {
			let baton = self.baton.lock ().await;
			baton.get ().map (|hold| hold.username.clone ())
		};
		
		let page = Page {
			holding_username,
		};
		
		self.template_response ("handlebars/commit.hbs", &page).await
	}
	
	async fn handle_git (&self, tail: &str) -> ResultResponse 
	{
		if tail == "" {
			bail! ("Need a path in the Git URL");
		}
		
		if tail == "info/refs" {
			return self.handle_git_info_refs ().await;
		}
		if tail == "objects/info/packs" {
			return self.handle_git_objects_info_packs ().await;
		}
		
		let bytes = tokio::fs::read (PathBuf::from (REPO_PATH).join (Path::new (".git")).join (tail)).await;
		
		if let Err (_) = bytes {
			return Ok (Response::builder ()
			.status (StatusCode::NOT_FOUND)
			.body (Body::empty ())?)
		}
		let bytes = bytes?;
		
		Ok (Response::builder ()
		.body (Body::from (bytes))?)
	}
	
	async fn handle_git_info_refs (&self) -> ResultResponse {
		let repo = Repository::open (REPO_PATH)?;
		
		let mut body = String::new ();
		
		let mut references: Vec <git2::Reference> = repo.references ()?.collect::<Result <Vec <git2::Reference>, _>> ()?;
		references.sort_by_key (|r| r.name ().map (|s| s.to_string ()));
		
		for reference in &references {
			body.push_str (&format! ("{}\t{}\n", reference.resolve ()?.target ().unwrap (), reference.name ().unwrap ()));
		}
		
		Ok (Response::builder ()
		.body (Body::from (body))?)
	}
	
	async fn handle_git_objects_info_packs (&self) -> ResultResponse {
		let mut iter = tokio::fs::read_dir (PathBuf::from (REPO_PATH).join (".git/objects/pack")).await.with_context (|| "Can't open .git/objects/pack")?;
		
		let mut body = String::new ();
		
		while let Some (entry) = iter.next_entry ().await? {
			let name = entry.file_name ();
			let name = name.to_str ().ok_or_else (|| anyhow::anyhow! ("file name isn't UTF-8"))?;
			
			if ! name.ends_with (".pack") {
				continue;
			}
			
			body.push_str (&format! ("P {}\n", name));
		}
		
		Ok (Response::builder ()
		.body (Body::from (body))?)
	}
	
	async fn handle_tree (&self, tail: &str) -> ResultResponse
	{
		let commit_id = (&tail [0..40]).to_string ();
		let tail = &tail [41..];
		
		let entries;
		
		{
			let repo = Repository::open (REPO_PATH)?;
			let oid = git2::Oid::from_str (&commit_id)
			.context ("Failed to make commit ID into OID")?;
			let commit = repo.find_commit (oid)
			.context ("Failed to find_commit")?;
			let tree = commit.tree ()
			.context ("Failed to get commit's tree")?;
			
			if tail == "" {
				entries = tree.iter ()
				.map (|entry| {
					let name = match entry.kind () {
						Some (git2::ObjectType::Tree) => entry.name ().map (|s| format! ("{}/", s)),
						_ => entry.name ().map (|s| s.to_string ()),
					};
					
					EntryData {
						name,
					}
				})
				.collect ();
			}
			else {
				let obj = tree.get_path (&Path::new (tail))
				.context ("Failed to get_path on tree")?
				.to_object (&repo)?;
				
				match obj.kind () {
					Some (git2::ObjectType::Tree) => {
						let tree = obj.into_tree ().map_err (|_| anyhow! ("Failed into_tree"))?;
						
						entries = tree.iter ()
						.map (|entry| {
							let name = entry.name ().map (|s| s.to_string ());
							
							EntryData {
								name,
							}
						})
						.collect ();
					},
					Some (git2::ObjectType::Blob) => {
						let blob = obj.into_blob ().map_err (|_| anyhow! ("Failed into_blob"))?;
						
						let bytes = blob.content ().to_vec ();
						
						return Ok (Response::builder ()
						.body (Body::from (bytes))?);
					},
					_ => bail! ("Git object is unknown type"),
				}
			}
		}
		
		let page = CommitPage {
			commit_id,
			entries,
		};
		
		self.template_response ("handlebars/tree.hbs", &page).await
	}
	
	async fn handle_static (&self, path: &str) -> ResultResponse
	{
		let bytes = tokio::fs::read (Path::new ("static/").join (path)).await
		.with_context (|| format! ("Failed to open static file"))?;
		
		Ok (Response::builder ()
		.body (Body::from (bytes))?)
	}
	
	async fn template_response <P: AsRef <Path> + Debug, T: Serialize> (
		&self,
		path: P,
		data: &T
	) -> ResultResponse 
	{
		let template = tokio::fs::read_to_string (&path).await
		.with_context (|| format! ("Failed to load Handlebars template `{:?}`", &path))?;
		let body = self.handlebars.render_template (&template, data)
		.with_context (|| format! ("Failed to render Handlebars template `{:?}`", &path))?;
		
		let bytes = body.as_bytes ().to_vec ();
		
		Ok (Response::builder ()
		.body (Body::from (bytes))?)
	}
}

struct CommitData {
	id: String,
	author: Option <String>,
	time: String,
	message: Option <String>,
}

fn get_last_commits (n: usize) -> anyhow::Result <Vec <CommitData>> {
	use chrono::{DateTime, NaiveDateTime, Utc};
	
	let mut commits = vec! [];
	
	// I want the repo to be dropped before I start rendering the body
	let repo = Repository::open (REPO_PATH)?;
	
	let head = repo.head ()?;
	let mut commit = head.peel_to_commit ()?;
	
	let replacer = gh_emoji::Replacer::new ();
	
	for _ in 0..n {
		let author = commit.author ().name ().map (|s| s.to_string ());
		
		let time = commit.time ();
		let time = DateTime::<Utc>::from_utc (NaiveDateTime::from_timestamp (time.seconds (), 0), Utc);
		
		let message = commit.message ()
		.map (|s| replacer.replace_all (s).to_string ());
		
		commits.push (CommitData {
			id: commit.id ().to_string (),
			author,
			time: time.to_string (),
			message,
		});
		
		if commit.parent_count () != 1 {
			// println! ("Error: Not one parent");
			break;
		}
		else {
			commit = commit.parent (0)?;
		}
	}
	
	Ok (commits)
}

async fn read_body_limited (mut body: Body, limit: usize) -> anyhow::Result <Vec <u8>>
{
	use futures_util::StreamExt;
	
	let mut buffer = vec! [];
	while let Some (chunk) = body.next ().await {
		let chunk = chunk?;
		
		if buffer.len () + chunk.len () > limit {
			bail! ("Body was bigger than limit");
		}
		
		buffer.extend_from_slice (&chunk);
	}
	
	Ok (buffer)
}

fn handle_error (status_code: StatusCode, s: &'static str) -> ResultResponse {
	Ok (Response::builder ()
	.status (status_code)
	.body (Body::from (s))?)
}

#[derive (Debug, PartialEq)]
enum BotCommand {
	Help,
	GetStatus,
	GetLastCommit,
}

fn parse_irc_privmsg (robot_nick: &str, line: &str) -> Option <BotCommand> 
{
	use BotCommand::*;
	
	let line = if let Some (line) = line.strip_prefix (robot_nick) {
		if let Some (line) = line.strip_prefix (": ") {
			line
		}
		else if let Some (line) = line.strip_prefix (" ") {
			line
		}
		else {
			return None;
		}
	}
	else if let Some (line) = line.strip_prefix ("! ") {
		line
	}
	else if let Some (line) = line.strip_prefix ("!") {
		line
	}
	else {
		return None;
	};
	
	match line {
		"help" => Some (Help),
		"info" => Some (Help),
		"status" => Some (GetStatus),
		"head" => Some (GetLastCommit),
		_ => None,
	}
}

#[cfg (test)]
mod tests {
	#[test]
	fn irc () {
		use super::BotCommand::*;
			
		for (input, expected) in vec! [
			("I'm just talking about the bot", None),
			("The bot can dig it!", None),
			("bot: help", Some (Help)),
			("bot: info", Some (Help)),
			("bot help", Some (Help)),
			("!help", Some (Help)),
			("!status", Some (GetStatus)),
			("bot: status", Some (GetStatus)),
		].into_iter () {
			let actual = super::parse_irc_privmsg ("bot", input);
			assert_eq! (actual, expected);
		}
	}
}
