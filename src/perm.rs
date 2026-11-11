//! Team / project level access. Platform permissions live on `auth::User`.
use crate::db::query_as;
use crate::auth::User;
use crate::db::Db;
use crate::error::{AppError, Res};

pub fn require(ok: bool) -> Res<()> {
    if ok {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub struct TeamAccess {
    /// Team admin, or a platform admin holding `teams.manage`.
    pub admin: bool,
    pub member: bool,
}

pub async fn team_access(db: &Db, user: &User, team_id: i64) -> Res<TeamAccess> {
    let exists: Option<(i64,)> = query_as("SELECT id FROM teams WHERE id = ?").bind(team_id).fetch_optional(db).await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    let role: Option<(String,)> = query_as("SELECT role FROM team_members WHERE team_id = ? AND user_id = ?")
        .bind(team_id)
        .bind(user.id)
        .fetch_optional(db)
        .await?;
    let access = TeamAccess {
        admin: user.has("teams.manage") || role.as_ref().is_some_and(|r| r.0 == "admin"),
        member: role.is_some(),
    };
    require(access.admin || access.member)?;
    Ok(access)
}

/// What the current user may do inside one project (and all of its boards).
pub struct Access {
    pub team_id: i64,
    pub project_id: i64,
    pub team_admin: bool,
    /// Has joined the project.
    pub member: bool,
    /// Manage boards, columns, labels and agent settings.
    pub manage: bool,
    pub create_cards: bool,
    pub comment: bool,
    pub edit_own: bool,
    pub edit_all: bool,
}

impl Access {
    pub fn can_edit_card(&self, created_by: Option<i64>, user_id: i64) -> bool {
        self.edit_all || (self.edit_own && created_by == Some(user_id))
    }
    /// Participants (as opposed to read-only team members) may use the agent console and upload files.
    pub fn participant(&self) -> bool {
        self.member || self.manage
    }
}

/// Fails with Forbidden unless the user may at least view the project.
pub async fn project_access(db: &Db, user: &User, project_id: i64) -> Res<Access> {
    let (team_id,): (i64,) = query_as("SELECT team_id FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    let team = team_access(db, user, team_id).await?;
    let pm: Option<(bool, bool, bool, bool, bool)> = query_as(
        "SELECT can_manage, can_create_cards, can_comment, can_edit_own, can_edit_all FROM project_members WHERE project_id = ? AND user_id = ?",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_optional(db)
    .await?;
    let member = pm.is_some();
    let (m, create, comment, own, all) = pm.unwrap_or_default();
    let manage = team.admin || m;
    let edit_all = manage || all;
    Ok(Access {
        team_id,
        project_id,
        team_admin: team.admin,
        member,
        manage,
        create_cards: manage || create,
        comment: manage || comment,
        edit_own: edit_all || own,
        edit_all,
    })
}

pub async fn board_access(db: &Db, user: &User, board_id: i64) -> Res<Access> {
    let (project_id,): (i64,) = query_as("SELECT project_id FROM boards WHERE id = ?")
        .bind(board_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)?;
    project_access(db, user, project_id).await
}
