use crate::{
    entities::{SignInParams, SignUpParams, UpdateUserParams, UserDetail},
    errors::{ErrorBuilder, ErrorCode, UserError},
    services::user::{construct_user_server, database::UserDB, UserServerAPI},
    sql_tables::{UserTable, UserTableChangeset},
};

use flowy_database::{
    query_dsl::*,
    schema::{user_table, user_table::dsl},
    DBConnection,
    ExpressionMethods,
    UserDatabaseConnection,
};

use crate::entities::UserToken;
use flowy_infra::kv::KVStore;
use flowy_sqlite::ConnectionPool;
use std::sync::{Arc, RwLock};

pub struct UserSessionConfig {
    root_dir: String,
}

impl UserSessionConfig {
    pub fn new(root_dir: &str) -> Self {
        Self {
            root_dir: root_dir.to_owned(),
        }
    }
}

type Server = Arc<dyn UserServerAPI + Send + Sync>;

pub struct UserSession {
    database: UserDB,
    config: UserSessionConfig,
    #[allow(dead_code)]
    pub(crate) server: Server,
    user_id: RwLock<Option<String>>,
}

impl UserSession {
    pub fn new(config: UserSessionConfig) -> Self {
        let db = UserDB::new(&config.root_dir);
        let server = construct_user_server();
        Self {
            database: db,
            config,
            server,
            user_id: RwLock::new(None),
        }
    }

    pub fn get_db_connection(&self) -> Result<DBConnection, UserError> {
        let user_id = self.user_id()?;
        self.database.get_connection(&user_id)
    }

    pub fn db_connection_pool(&self) -> Result<Arc<ConnectionPool>, UserError> {
        let user_id = self.user_id()?;
        self.database.get_pool(&user_id)
    }

    pub async fn sign_in(&self, params: SignInParams) -> Result<UserTable, UserError> {
        let resp = self.server.sign_in(params).await?;
        let _ = self.set_user_id(Some(resp.uid.clone()))?;
        let user_table = self.save_user(resp.into()).await?;

        Ok(user_table)
    }

    pub async fn sign_up(&self, params: SignUpParams) -> Result<UserTable, UserError> {
        let resp = self.server.sign_up(params).await?;
        let _ = self.set_user_id(Some(resp.uid.clone()))?;
        let user_table = self.save_user(resp.into()).await?;

        Ok(user_table)
    }

    pub async fn sign_out(&self) -> Result<(), UserError> {
        let user_detail = self.user_detail().await?;

        match self.server.sign_out(&user_detail.token).await {
            Ok(_) => {},
            Err(e) => log::error!("Sign out failed: {:?}", e),
        }

        let conn = self.get_db_connection()?;
        let _ =
            diesel::delete(dsl::user_table.filter(dsl::id.eq(&user_detail.id))).execute(&*conn)?;
        let _ = self.server.sign_out(&user_detail.id);
        let _ = self.database.close_user_db(&user_detail.id)?;
        let _ = self.set_user_id(None)?;

        Ok(())
    }

    async fn save_user(&self, user: UserTable) -> Result<UserTable, UserError> {
        let conn = self.get_db_connection()?;
        let _ = diesel::insert_into(user_table::table)
            .values(user.clone())
            .execute(&*conn)?;

        Ok(user)
    }

    pub async fn update_user(&self, params: UpdateUserParams) -> Result<(), UserError> {
        let changeset = UserTableChangeset::new(params);
        let conn = self.get_db_connection()?;
        diesel_update_table!(user_table, changeset, conn);
        Ok(())
    }

    pub async fn user_detail(&self) -> Result<UserDetail, UserError> {
        let user_id = self.user_id()?;
        let user = dsl::user_table
            .filter(user_table::id.eq(&user_id))
            .first::<UserTable>(&*(self.get_db_connection()?))?;

        let server = self.server.clone();
        let token = user.token.clone();
        tokio::spawn(async move {
            match server.get_user_detail(&token).await {
                Ok(user_detail) => {
                    //
                    log::info!("{:?}", user_detail);
                },
                Err(e) => {
                    //
                    log::info!("{:?}", e);
                },
            }
        })
        .await;

        Ok(UserDetail::from(user))
    }

    pub fn set_user_id(&self, user_id: Option<String>) -> Result<(), UserError> {
        log::trace!("Set user id: {:?}", user_id);
        KVStore::set_str(USER_ID_CACHE_KEY, user_id.clone().unwrap_or("".to_owned()));
        match self.user_id.write() {
            Ok(mut write_guard) => {
                *write_guard = user_id;
                Ok(())
            },
            Err(e) => Err(ErrorBuilder::new(ErrorCode::WriteCurrentIdFailed)
                .error(e)
                .build()),
        }
    }

    pub fn user_dir(&self) -> Result<String, UserError> {
        let user_id = self.user_id()?;
        Ok(format!("{}/{}", self.config.root_dir, user_id))
    }

    pub fn user_id(&self) -> Result<String, UserError> {
        let mut user_id = {
            let read_guard = self.user_id.read().map_err(|e| {
                ErrorBuilder::new(ErrorCode::ReadCurrentIdFailed)
                    .error(e)
                    .build()
            })?;

            (*read_guard).clone()
        };

        if user_id.is_none() {
            user_id = KVStore::get_str(USER_ID_CACHE_KEY);
            let _ = self.set_user_id(user_id.clone())?;
        }

        match user_id {
            None => Err(ErrorBuilder::new(ErrorCode::UserNotLoginYet).build()),
            Some(user_id) => Ok(user_id),
        }
    }

    // pub fn user_token(&self) -> Result<String, UserError> {
    //     let user_detail = self.user_detail()?;
    //     Ok(user_detail.token)
    // }
}

pub async fn update_user(
    server: Server,
    pool: Arc<ConnectionPool>,
    params: UpdateUserParams,
) -> Result<(), UserError> {
    let changeset = UserTableChangeset::new(params);
    let conn = pool.get()?;
    diesel_update_table!(user_table, changeset, conn);
    Ok(())
}

pub fn current_user_id() -> Result<String, UserError> {
    match KVStore::get_str(USER_ID_CACHE_KEY) {
        None => Err(ErrorBuilder::new(ErrorCode::UserNotLoginYet).build()),
        Some(user_id) => Ok(user_id),
    }
}

impl UserDatabaseConnection for UserSession {
    fn get_connection(&self) -> Result<DBConnection, String> {
        self.get_db_connection().map_err(|e| format!("{:?}", e))
    }
}

const USER_ID_CACHE_KEY: &str = "user_id";
