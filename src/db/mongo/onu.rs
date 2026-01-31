use super::MongoDB;
use crate::db::OnuRepository;
use crate::models::db::{OnuForUpdateIp, OnuIdentity, OnuIpUpdate};

use crate::services::zte_parse_update::OnuDetected;
use async_trait::async_trait;
use futures::TryStreamExt;
use mongodb::{
    bson::{doc, Document},
    Collection,
};

#[async_trait]
impl OnuRepository for MongoDB {
    async fn get_device_serial_numbers(&self) -> Result<Vec<OnuIdentity>, String> {
        // 1. Apunta a la colección correcta.
        let collection: Collection<Document> = self.db.collection("Onus");

        // 2. Proyección: Solo traer _id y sSn (ahorra memoria y ancho de banda)
        let mut cursor = collection
            .find(doc! {})
            .projection(doc! {
                "_id": 1,
                "sSn": 1,
                "sMac": 1,
                "nMotherboard": 1,
                "nPon": 1,
                "nIdOnu": 1,
                "idOlt": 1,
            })
            .await
            .map_err(|e| e.to_string())?;

        let mut devices = Vec::new();

        // 3. Iterar el cursor de forma asíncrona
        while let Some(doc) = cursor.try_next().await.map_err(|e| e.to_string())? {
            // Extraemos el _id (ObjectId) y lo dejamos como ObjectId
            let id = doc.get_object_id("_id");
            let sn = doc.get_str("sSn");
            let mac = doc.get_str("sMac");
            let motherboard = doc.get_i32("nMotherboard");
            let pon = doc.get_i32("nPon");
            let id_onu = doc.get_i32("nIdOnu");
            let id_olt = doc.get_object_id("idOlt");

            if let (Ok(id), Ok(sn)) = (id, sn) {
                devices.push(OnuIdentity {
                    id,
                    sn: sn.to_string(),
                    mac: mac.ok().map(|s| s.to_string()),
                    motherboard: motherboard.ok(),
                    pon: pon.ok(),
                    id_onu: id_onu.ok(),
                    id_olt: id_olt.ok(),
                });
            }
        }

        Ok(devices)
    }

    async fn save_onu_from_zte(&self, onu: OnuDetected, id_editor: &str) -> Result<(), String> {
        // Placeholder implementation
        // Upsert or Insert the serial number
        let collection: Collection<Document> = self.db.collection("Onus");

        // Example: Update the "last_checked" or similar, or just ensure it exists
        // The user said "guardar las sn en la db", so maybe we are scraping them FROM ZTE and saving locally?
        // If so, upsert is best.

        let filter = doc! { "_id": &onu.id };
        let update = doc! {
            "$set": {
                "sMac": &onu.mac,
                "nMotherboard": &onu.motherboard,
                "nPon": &onu.pon,
                "nIdOnu": &onu.id_onu,
                "idOlt": &onu.id_olt,
                "dEdition": mongodb::bson::DateTime::now(),
                "idEditor": id_editor
            }
        };
        let options = mongodb::options::UpdateOptions::builder()
            .upsert(true)
            .build();

        collection
            .update_one(filter, update)
            .with_options(options)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    async fn get_onus_for_update_ip(&self) -> Result<Vec<OnuForUpdateIp>, String> {
        let collection = self.db.collection::<OnuForUpdateIp>("Onus");
        let filter = doc! { "sMac": { "$exists": true, "$ne": null } };

        let cursor = collection
            .find(filter)
            .projection(doc! {
                "_id": 1,
                "sMac": 1,
                "sIp": 1,
            })
            .await
            .map_err(|e| e.to_string())?;

        let onus = cursor.try_collect().await.map_err(|e| e.to_string())?;
        Ok(onus)
    }

    async fn update_onu_ip(&self, onu: OnuIpUpdate, id_editor: &str) -> Result<(), String> {
        let collection = self.db.collection::<OnuForUpdateIp>("Onus");
        let filter = doc! { "_id": onu.id };
        let update = doc! {
            "$set": {
                "sIp": onu.new_ip,
                "dEdition": mongodb::bson::DateTime::now(),
                "idEditor": id_editor
            }
        };
        collection
            .update_one(filter, update)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
