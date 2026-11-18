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
        fn handle(&self, user: &str, method: &str, params: &Value) -> Result<Value, String> {
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
