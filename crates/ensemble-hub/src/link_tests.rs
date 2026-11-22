//! The runner's side of the link (`ensemble_link::client`) against this hub.

mod link {
    use crate::server::testing::start;
    use ensemble_link::{Link, Token};

    #[tokio::test(flavor = "multi_thread")]
    async fn hello_says_who_the_token_is_and_a_bad_token_is_named() {
        let hub = start();
        let hello = Link::new(&hub.url, &hub.ana).hello().await.unwrap();
        assert_eq!(
            (hello.user.as_str(), hello.role.as_str()),
            ("ana", "member")
        );
        assert!(
            hello.permissions.contains(&"share".to_string()),
            "{hello:?}"
        );
        let rex = Link::new(&hub.url, &hub.rex).hello().await.unwrap();
        assert_eq!(rex.permissions, vec!["recall"]);

        let err = Link::new(&hub.url, "ens_wrong").hello().await.unwrap_err();
        assert!(err.contains("did not accept the token"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_token_is_used_from_the_next_request_on() {
        let hub = start();
        let token = Token::new("ens_wrong");
        let link = Link::with_token(&hub.url, token.clone());
        assert!(link.hello().await.is_err());
        assert!(token.set(&hub.rex));
        assert!(!token.set(&hub.rex), "the same token is no change");
        assert_eq!(link.hello().await.unwrap().user, "rex");
        token.set(&hub.ana);
        assert_eq!(link.hello().await.unwrap().user, "ana");
    }
}

mod signing_in {
    use crate::server::testing::{PASSWORD, start};
    use crate::users::Users;
    use ensemble_link::Link;
    use ensemble_link::client::{Failure, sign_in};
    use ensemble_link::protocol::{JOIN, LOGIN};
    use ensemble_ops::Caller;
    use serde_json::json;

    fn login(
        url: &str,
        name: &str,
        password: &str,
    ) -> impl Future<Output = Result<String, Failure>> {
        sign_in(
            url,
            LOGIN,
            json!({"name": name, "password": password, "client": "runner desk"}),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_logs_in_with_a_password_for_a_token_named_after_it() {
        let hub = start();
        let token = login(&hub.url, "ana", PASSWORD).await.unwrap();
        let hello = Link::new(&hub.url, &token).hello().await.unwrap();
        assert_eq!(hello.user, "ana");
        let users = Users::load(&hub.tmp.0.join("state/core")).unwrap();
        let tokens = users.tokens("ana").unwrap();
        assert!(tokens.iter().any(|t| t.name == "runner desk"), "{tokens:?}");
        assert!(
            users
                .get("ana")
                .unwrap()
                .tokens
                .iter()
                .all(|t| t.kind != crate::users::Kind::Session),
            "no session is left behind"
        );

        let err = login(&hub.url, "ana", "not the password")
            .await
            .unwrap_err();
        assert_eq!(err, Failure::Hub("wrong name or password".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failed_logins_over_the_link_lock_the_name_as_anywhere() {
        let hub = start();
        // The test hub locks a name after 3 failures.
        for _ in 0..3 {
            assert!(login(&hub.url, "rex", "wrong password!!").await.is_err());
        }
        let err = login(&hub.url, "rex", PASSWORD)
            .await
            .unwrap_err()
            .message();
        assert!(err.contains("too many failed logins"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_joins_with_a_code_once_before_it_expires() {
        let hub = start();
        let (operations, context) = hub.server.operations();
        let made = operations
            .run(
                context,
                &Caller::Local,
                "user.join-code",
                json!({"name": "rex"}),
            )
            .unwrap();
        let code = made["code"].as_str().unwrap().to_string();
        assert!(
            made["note"]
                .as_str()
                .unwrap()
                .contains("ensemble-runner join <hub-url> join_"),
            "{made}"
        );
        let join = |code: &str| {
            sign_in(
                &hub.url,
                JOIN,
                json!({"code": code, "client": "runner lab"}),
            )
        };
        let token = join(&code).await.unwrap();
        assert_eq!(
            Link::new(&hub.url, &token).hello().await.unwrap().user,
            "rex"
        );
        let spent = join(&code).await.unwrap_err().message();
        assert!(spent.contains("join code is not valid"), "{spent}");
        let unknown = join("join_0000").await.unwrap_err().message();
        assert!(unknown.contains("join code is not valid"), "{unknown}");

        // A code a day old is refused, and gone once the accounts are written.
        let made = operations
            .run(
                context,
                &Caller::Local,
                "user.join-code",
                json!({"name": "rex"}),
            )
            .unwrap();
        let code = made["code"].as_str().unwrap();
        let state = hub.tmp.0.join("state/core");
        let mut users = Users::load(&state).unwrap();
        users.expire_join_codes("rex");
        users.save().unwrap();
        let expired = join(code).await.unwrap_err().message();
        assert!(expired.contains("join code is not valid"), "{expired}");
    }
}

mod tasks {
    use crate::board::TaskService;
    use crate::server::testing::start_with;
    use ensemble_link::Link;
    use ensemble_link::protocol::{TASK_METHODS, TASKS};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    /// What runners asked of the tasks.
    #[derive(Default)]
    struct FakeTasks {
        heard: Mutex<Vec<(String, String, Value)>>,
    }

    impl TaskService for FakeTasks {
        fn handle(
            &self,
            asker: &crate::board::Asker<'_>,
            method: &str,
            params: &Value,
        ) -> Result<Value, String> {
            let user = asker.user;
            self.heard
                .lock()
                .unwrap()
                .push((user.into(), method.into(), params.clone()));
            Ok(json!({ "tasks": [] }))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn task_requests_reach_the_service_as_the_signed_in_account() {
        let tasks: &'static FakeTasks = Box::leak(Box::default());
        let hub = start_with(Some(tasks));
        let link = Link::new(&hub.url, &hub.ana);
        let v = link
            .ask(TASKS, json!({"project": "shop", "owner": "mallory"}))
            .await
            .unwrap();
        assert_eq!(v, json!({ "tasks": [] }));
        let heard = tasks.heard.lock().unwrap().clone();
        assert_eq!(
            heard,
            vec![(
                "ana".to_string(),
                TASKS.to_string(),
                json!({"project": "shop", "owner": "mallory"})
            )],
            "the account is the token's, never one the runner sends"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_without_the_tasks_permission_reaches_no_task() {
        let tasks: &'static FakeTasks = Box::leak(Box::default());
        let hub = start_with(Some(tasks));
        let link = Link::new(&hub.url, &hub.rex);
        for method in TASK_METHODS {
            let err = link
                .ask(method, json!({"project": "shop"}))
                .await
                .unwrap_err();
            assert!(err.contains("do not allow `tasks`"), "{method}: {err}");
        }
        assert!(tasks.heard.lock().unwrap().is_empty());
    }
}
