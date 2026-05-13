use async_trait::async_trait;
use futures::stream::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::options::{FindOptions, UpdateOptions};
use mongodb::Collection;

use super::MongoDB;
use crate::db::{UpdateUserPatch, UserListFilter, UserRepository};
use crate::models::users::{User, UserCredentials};

/// Escapa caracteres especiales de regex para usar un string como literal.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".*+?^${}()|[]\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Codifica `(sName, _id)` como cursor opaco para paginación estable.
/// Formato: `"{sName}\x1f{id}"` → base64 (pipe-char sin colisión).
fn encode_user_cursor(name: &str, id: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(format!("{}\x1f{}", name, id))
}

fn decode_user_cursor(c: &str) -> Option<(String, String)> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let bytes = URL_SAFE_NO_PAD.decode(c).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let (name, id) = s.split_once('\x1f')?;
    Some((name.to_string(), id.to_string()))
}

pub(crate) fn last_user_cursor(users: &[User]) -> Option<String> {
    users.last().map(|u| encode_user_cursor(&u.name, &u.id))
}

#[async_trait]
impl UserRepository for MongoDB {
    async fn find_user_by_email(&self, email: &str) -> Result<Option<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        let filter = doc! { "email": email };

        match collection.find_one(filter).await {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::error!("❌ Error finding user by email {}: {:?}", email, e);
                Err(e.to_string())
            }
        }
    }

    async fn find_user_credentials_by_user_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserCredentials>, String> {
        let collection: Collection<UserCredentials> = self.db.collection("UserCredentials");
        let filter = doc! { "userId": user_id };

        match collection.find_one(filter).await {
            Ok(res) => Ok(res),
            Err(e) => {
                tracing::error!(
                    "❌ Error finding credentials for user_id {}: {:?}",
                    user_id,
                    e
                );
                Err(e.to_string())
            }
        }
    }

    async fn find_user_by_id(&self, id: &str) -> Result<Option<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        // LB4 uses string IDs (UUIDs)
        let filter = doc! { "_id": id };

        collection.find_one(filter).await.map_err(|e| e.to_string())
    }

    async fn create_user(&self, user: User) -> Result<(), String> {
        let collection: Collection<User> = self.db.collection("Users");
        collection
            .insert_one(user)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn create_user_credentials(&self, creds: UserCredentials) -> Result<(), String> {
        let collection: Collection<UserCredentials> = self.db.collection("UserCredentials");
        collection
            .insert_one(creds)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn find_agents(&self) -> Result<Vec<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        // nRole >= 0 AND nRole < 3 (excluye providers con nRole == 3.0)
        // `bIsBot` distinto de true → mantiene fuera al user sintético del AI Agent.
        let filter = doc! {
            "nRole": { "$gte": 0.0, "$lt": 3.0 },
            "visible": true,
            "bIsBot": { "$ne": true },
        };

        collection
            .find(filter)
            .sort(doc! { "sName": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_chat_agents(&self) -> Result<Vec<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        // El bot tiene `bCanChat = false` por construcción; `bIsBot != true`
        // es defensa adicional por si en el futuro alguien crea otro user con
        // can_chat habilitado por error.
        let filter = doc! {
            "bCanChat": true,
            "visible": true,
            "bIsBot": { "$ne": true },
        };

        collection
            .find(filter)
            .sort(doc! { "sName": 1 })
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_providers(&self) -> Result<Vec<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        let filter = doc! { "nRole": 3.0 };

        let mut cursor = collection
            .find(filter)
            .sort(doc! { "nTag": 1})
            .await
            .map_err(|e| e.to_string())?;

        let mut users = Vec::new();
        while let Some(user) = cursor.try_next().await.map_err(|e| e.to_string())? {
            users.push(user);
        }
        Ok(users)
    }

    async fn set_user_visible(&self, id: &str, visible: bool) -> Result<bool, String> {
        let collection: Collection<User> = self.db.collection("Users");

        // Aggregation pipeline update (Mongo 4.2+): permite lógica condicional
        // atómica en una sola llamada. Evita race entre "leer role actual +
        // escribir nRolePrev/nRole" que tendría dos operaciones separadas.
        let pipeline: Vec<mongodb::bson::Document> = if visible {
            // REACTIVAR:
            // - Si el role actual es -1 (desactivado), restauramos desde
            //   `nRolePrev` (o 1.0 si no existe) y borramos `nRolePrev`.
            // - Si ya estaba activo (role != -1), no tocamos role ni nRolePrev.
            vec![doc! { "$set": {
                "visible": true,
                "nRole": {
                    "$cond": {
                        "if": { "$eq": ["$nRole", -1.0] },
                        "then": { "$ifNull": ["$nRolePrev", 1.0] },
                        "else": "$nRole",
                    }
                },
                "nRolePrev": {
                    "$cond": {
                        "if": { "$eq": ["$nRole", -1.0] },
                        "then": "$$REMOVE",
                        "else": "$nRolePrev",
                    }
                },
            } }]
        } else {
            // DESACTIVAR:
            // - Si el role actual NO es -1, guardamos el role en `nRolePrev`
            //   y seteamos role a -1 (sin acceso).
            // - Si ya era -1 (idempotente), conservamos el `nRolePrev` previo.
            vec![doc! { "$set": {
                "visible": false,
                "nRolePrev": {
                    "$cond": {
                        "if": { "$ne": ["$nRole", -1.0] },
                        "then": "$nRole",
                        "else": "$nRolePrev",
                    }
                },
                "nRole": -1.0,
            } }]
        };

        let res = collection
            .update_one(doc! { "_id": id }, pipeline)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.matched_count > 0)
    }

    async fn update_user_password(
        &self,
        user_id: &str,
        password_hash: &str,
    ) -> Result<bool, String> {
        let users: Collection<User> = self.db.collection("Users");
        // `count_documents` evita deserializar el User (una projection `_id: 1`
        // sobre una Collection tipada falla pidiendo los campos obligatorios).
        let exists = users
            .count_documents(doc! { "_id": user_id })
            .limit(1)
            .await
            .map_err(|e| e.to_string())?
            > 0;
        if !exists {
            return Ok(false);
        }

        let creds: Collection<UserCredentials> = self.db.collection("UserCredentials");
        creds
            .update_one(
                doc! { "userId": user_id },
                doc! { "$set": { "password": password_hash, "userId": user_id } },
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
            .map_err(|e| e.to_string())?;
        Ok(true)
    }

    async fn update_user(&self, id: &str, patch: UpdateUserPatch) -> Result<bool, String> {
        let collection: Collection<User> = self.db.collection("Users");
        let mut set = Document::new();
        if let Some(n) = patch.name {
            set.insert("sName", n);
        }
        if let Some(e) = patch.email {
            set.insert("email", e);
        }
        if let Some(r) = patch.role {
            set.insert("nRole", r as f64);
        }
        if let Some(c) = patch.can_chat {
            set.insert("bCanChat", c);
        }
        if let Some(t) = patch.tag {
            set.insert("nTag", t as i64);
        }

        // Si el patch vino vacío, evitamos round-trip de update y sólo
        // chequeamos existencia (sin deserializar el User con una projection
        // parcial — `count_documents` es la forma segura).
        if set.is_empty() {
            let exists = collection
                .count_documents(doc! { "_id": id })
                .limit(1)
                .await
                .map_err(|e| e.to_string())?
                > 0;
            return Ok(exists);
        }

        let res = collection
            .update_one(doc! { "_id": id }, doc! { "$set": set })
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.matched_count > 0)
    }

    async fn list_users(&self, filter: UserListFilter<'_>) -> Result<Vec<User>, String> {
        let collection: Collection<User> = self.db.collection("Users");

        let mut q = Document::new();

        // Search: substring case-insensitive contra sName | email.
        // Se guarda el $or del search en una lista para combinarlo con el $or
        // del cursor al final usando $and (Mongo no permite 2 $or sueltos).
        let mut or_groups: Vec<Vec<Document>> = Vec::new();

        if let Some(s) = filter.search {
            let re = regex_escape(s.trim());
            if !re.is_empty() {
                or_groups.push(vec![
                    doc! { "sName": { "$regex": &re, "$options": "i" } },
                    doc! { "email": { "$regex": &re, "$options": "i" } },
                ]);
            }
        }

        if let Some(r) = filter.role {
            q.insert("nRole", r as f64);
        }
        if let Some(v) = filter.visible {
            q.insert("visible", v);
        }
        if let Some(cc) = filter.can_chat {
            q.insert("bCanChat", cc);
        }

        if let Some(c) = filter.cursor {
            if let Some((name, id)) = decode_user_cursor(c) {
                or_groups.push(vec![
                    doc! { "sName": { "$gt": &name } },
                    doc! { "sName": &name, "_id": { "$gt": &id } },
                ]);
            }
        }

        // Combinar múltiples $or: si hay 1, va directo; si hay más, se envuelven en $and.
        match or_groups.len() {
            0 => {}
            1 => {
                q.insert(
                    "$or",
                    or_groups
                        .pop()
                        .unwrap()
                        .into_iter()
                        .map(Bson::Document)
                        .collect::<Vec<_>>(),
                );
            }
            _ => {
                let and_arr: Vec<Bson> = or_groups
                    .into_iter()
                    .map(|g| {
                        let mut d = Document::new();
                        d.insert("$or", g.into_iter().map(Bson::Document).collect::<Vec<_>>());
                        Bson::Document(d)
                    })
                    .collect();
                q.insert("$and", and_arr);
            }
        }

        let opts = FindOptions::builder()
            .sort(doc! { "sName": 1, "_id": 1 })
            .limit(filter.limit)
            .build();

        collection
            .find(q)
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())
    }

    // ── realtime-pending-badges: T07 ─────────────────────────────────────────

    async fn find_users_by_roles(&self, roles: &[f32]) -> Result<Vec<String>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        // Convert &[f32] to Vec<Bson> for the $in operator.
        let roles_bson: Vec<mongodb::bson::Bson> = roles
            .iter()
            .map(|&r| mongodb::bson::Bson::Double(r as f64))
            .collect();
        let filter = doc! {
            "nRole": { "$in": roles_bson },
            "visible": true,
            "bIsBot": { "$ne": true },
        };
        let users: Vec<User> = collection
            .find(filter)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())?;
        Ok(users.into_iter().map(|u| u.id).collect())
    }

    async fn find_chat_user_ids(&self) -> Result<Vec<String>, String> {
        let collection: Collection<User> = self.db.collection("Users");
        let filter = doc! {
            "bCanChat": true,
            "visible": true,
            "bIsBot": { "$ne": true },
        };
        let users: Vec<User> = collection
            .find(filter)
            .await
            .map_err(|e| e.to_string())?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| e.to_string())?;
        Ok(users.into_iter().map(|u| u.id).collect())
    }

    async fn get_user_role_and_can_chat(
        &self,
        user_id: &str,
    ) -> Result<(Option<f32>, bool), String> {
        let collection: Collection<Document> = self.db.collection("Users");
        let opts = FindOptions::builder()
            .projection(doc! { "_id": 0, "nRole": 1, "bCanChat": 1 })
            .limit(1)
            .build();
        let result = collection
            .find(doc! { "_id": user_id })
            .with_options(opts)
            .await
            .map_err(|e| e.to_string())?
            .try_next()
            .await
            .map_err(|e| e.to_string())?;

        match result {
            None => Ok((None, false)),
            Some(doc) => {
                let role = doc.get("nRole").and_then(|v| match v {
                    mongodb::bson::Bson::Double(d) => Some(*d as f32),
                    mongodb::bson::Bson::Int32(i) => Some(*i as f32),
                    mongodb::bson::Bson::Int64(i) => Some(*i as f32),
                    _ => None,
                });
                let can_chat = doc
                    .get("bCanChat")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok((role, can_chat))
            }
        }
    }
}
