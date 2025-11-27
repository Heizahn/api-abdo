use crate::utils::timezone::{utils as tz_utils, VenezuelaDateTime};
use async_trait::async_trait;
use futures::stream::{StreamExt, TryStreamExt};
use mongodb::bson::{doc, oid::ObjectId, DateTime, Document};
use mongodb::results::InsertOneResult;
use mongodb::{error::Error as MongoError, Collection};

use super::{MongoDB};
use crate::db::SalesRepository;
use crate::models::db::{Debt, PartPayment, Payment};
use crate::models::payment::{Bank, ClientOwner, PaymentMethod, PaymentReport, UserPaymentInfo};

#[async_trait]
impl SalesRepository for MongoDB {
    async fn get_latest_exchange_rate(&self) -> Result<f64, MongoError> {
        let db_bcv = self.client.database("BCV");
        let collection: Collection<Document> = db_bcv.collection("BCVRates");

        let start_of_day = tz_utils::start_of_today_venezuela();
        let start_of_day_display = start_of_day.clone();
        let start_of_day_bson = mongodb::bson::DateTime::from(start_of_day);

        let filter = doc! { "timestamp": { "$gte": start_of_day_bson } };
        let options = mongodb::options::FindOptions::builder()
            .sort(doc! { "timestamp": -1 })
            .limit(1)
            .build();

        let mut cursor = collection.find(filter).with_options(options).await?;
        let doc = cursor.try_next().await?;

        match doc {
            Some(d) => {
                let rate = d.get_f64("value").map_err(|_| {
                    MongoError::custom("Rate field 'value' not found or invalid type")
                })?;

                if let Ok(ts) = d.get_datetime("timestamp") {
                    let vz_time = VenezuelaDateTime::from(*ts);
                    tracing::info!(
                        "💱 Tasa BCV: {} @ {} (hora Venezuela)",
                        rate,
                        vz_time.datetime_string_venezuela()
                    );
                    tracing::debug!("💾 Timestamp en DB (UTC): {}", vz_time.utc());
                } else {
                    tracing::info!("💱 Tasa BCV encontrada: {}", rate);
                }
                Ok(rate)
            }
            None => {
                tracing::warn!(
                    "⚠️ No se encontró tasa BCV para hoy (desde {} Venezuela)",
                    start_of_day_display.datetime_string_venezuela()
                );
                Err(MongoError::custom(
                    "No exchange rate found for today in BCV collection",
                ))
            }
        }
    }


    async fn find_part_payments_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PartPayment>, String> {
        let filter = doc! { "idDebt": { "$in": debt_ids } };
        let collection = self.db.collection::<Document>("PartPayment");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut part_payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let pp = PartPayment {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                id_debt: doc
                    .get_object_id("idDebt")
                    .unwrap_or_else(|_| ObjectId::new()),
                id_payment: doc
                    .get_object_id("idPayment")
                    .unwrap_or_else(|_| ObjectId::new()),
                n_amount: doc.get_f64("nAmount").unwrap_or(0.0),
            };
            part_payments.push(pp);
        }
        Ok(part_payments)
    }

    async fn find_payments_by_ids(&self, payment_ids: &[ObjectId]) -> Result<Vec<Payment>, String> {
        let filter = doc! { "_id": { "$in": payment_ids } };
        let collection = self.db.collection::<Document>("Payments");
        let mut cursor = collection.find(filter).await.map_err(|e| e.to_string())?;
        let mut payments = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let payment = Payment {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                n_amount: doc
                    .get_f64("nAmount")
                    .or_else(|_| doc.get_i32("nAmount").map(|v| v as f64))
                    .or_else(|_| doc.get_i64("nAmount").map(|v| v as f64))
                    .unwrap_or(0.0),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                n_bs: doc
                    .get_f64("nBs")
                    .or_else(|_| doc.get_i32("nBs").map(|v| v as f64))
                    .or_else(|_| doc.get_i64("nBs").map(|v| v as f64))
                    .unwrap_or(0.0),
            };
            payments.push(payment);
        }
        Ok(payments)
    }

    async fn find_active_debts_by_client_ids(
        &self,
        client_ids: &[ObjectId],
    ) -> Result<Vec<Debt>, String> {
        let filter = doc! {
            "idClient": { "$in": client_ids },
            "sState": "Activo"
        };
        let sort_debts = doc! { "dCreation": 1 };
        let collection = self.db.collection::<Document>("Debts");
        let mut cursor = collection
            .find(filter)
            .sort(sort_debts)
            .await
            .map_err(|e| e.to_string())?;
        let mut debts = Vec::new();

        while let Some(Ok(doc)) = cursor.next().await {
            let debt = Debt {
                _id: doc.get_object_id("_id").unwrap_or_else(|_| ObjectId::new()),
                n_amount: doc
                    .get_f64("nAmount")
                    .or_else(|_| doc.get_i32("nAmount").map(|v| v as f64))
                    .or_else(|_| doc.get_i64("nAmount").map(|v| v as f64))
                    .unwrap_or(0.0),
                s_state: doc.get_str("sState").unwrap_or_default().to_string(),
                id_client: doc
                    .get_object_id("idClient")
                    .unwrap_or_else(|_| ObjectId::new()),
                s_reason: doc.get_str("sReason").unwrap_or_default().to_string(),
                d_creation: doc
                    .get_datetime("dCreation")
                    .ok()
                    .cloned()
                    .unwrap_or_else(|| DateTime::from_millis(0)),
            };
            debts.push(debt);
        }
        Ok(debts)
    }

    //[PAYMENT]
    async fn find_debt_by_id(&self, id: &str) -> Result<Option<Debt>, String> {
        let collection = self.db.collection::<Debt>("Debts");
        let obj_id = ObjectId::parse_str(id).map_err(|e| e.to_string())?;
        collection
            .find_one(doc! { "_id": obj_id })
            .await
            .map_err(|e| e.to_string())
    }

    async fn find_client_owner_by_id(
        &self,
        client_id: &ObjectId,
    ) -> Result<Option<ClientOwner>, String> {
        let collection = self.db.collection::<ClientOwner>("Clients");
        let options = mongodb::options::FindOneOptions::builder()
            .projection(doc! { "idOwner": 1 })
            .build();
        collection
            .find_one(doc! { "_id": client_id })
            .with_options(options)
            .await
            .map_err(|e| e.to_string())
    }

    // CAMBIO: Buscar User y traer idPaymentMethod
    async fn find_user_payment_info_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<UserPaymentInfo>, String> {
        let collection = self.db.collection::<UserPaymentInfo>("Users");
        let filter = doc! { "_id": user_id };
        let options = mongodb::options::FindOneOptions::builder()
            .projection(doc! { "idPaymentMethod": 1 }) // Solo traemos el ID del método
            .build();

        collection
            .find_one(filter)
            .with_options(options)
            .await
            .map_err(|e| e.to_string())
    }

    // CAMBIO: Buscar PaymentMethod por _id
    async fn find_payment_method_by_id(
        &self,
        id: &ObjectId,
    ) -> Result<Option<PaymentMethod>, String> {
        let collection = self.db.collection::<PaymentMethod>("PaymentMethods");
        let filter = doc! {
            "_id": id,
            "bActive": true // Mantenemos el filtro de activo por seguridad
        };

        collection.find_one(filter).await.map_err(|e| e.to_string())
    }

    async fn create_payment_report(
        &self,
        report: PaymentReport,
    ) -> Result<InsertOneResult, MongoError> {
        // Accede a la colección tipada con el struct
        let collection = self.db.collection::<PaymentReport>("PaymentReports");

        // Inserta el documento
        collection.insert_one(report).await
    }

    async fn find_bank_list(&self) -> Result<Vec<Bank>, String> {
        let collection = self.db.collection::<Bank>("ListBanks");

        let mut cursor = collection.find(doc! {}).await.map_err(|e| e.to_string())?;
        let mut banks = Vec::new();

        while let Some(Ok(bank)) = cursor.next().await {
            banks.push(bank);
        }

        Ok(banks)
    }

    async fn find_pending_reports_by_debt_ids(
        &self,
        debt_ids: &[ObjectId],
    ) -> Result<Vec<PaymentReport>, String> {
        let collection = self.db.collection::<PaymentReport>("PaymentReports");

        // Buscamos reportes que coincidan con la lista de deudas Y que estén pendientes
        let filter = doc! {
            "idDebt": { "$in": debt_ids },
            "sState": "Pendiente"
        };

        let cursor = collection.find(filter).await.map_err(|e| e.to_string());
        let mut reports = Vec::new();

        let mut cursor = cursor?;
        while let Some(Ok(report)) = cursor.next().await {
            reports.push(report);
        }

        Ok(reports)
    }
}
