use std::fs;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};

use crate::now_shanghai;
use crate::statistics_model::Statistics;
use crate::{boring_face::BoringFace, DbPool};

use crate::membership_model::Membership;
use anyhow::anyhow;
use chrono::{NaiveDateTime, NaiveTime};
use headers::HeaderMap;
use lazy_static::lazy_static;
use regex::Regex;
use serde::Serialize;
use tokio::sync::watch::{self, Receiver, Sender};
use tokio::sync::RwLock;
use tracing::info;

pub type DynContext = Arc<Context>;

lazy_static! {
    static ref IPV4_MASK: Regex = Regex::new("(\\d*\\.).*(\\.\\d*)").unwrap();
    static ref IPV6_MASK: Regex = Regex::new("(\\w*:\\w*:).*(:\\w*:\\w*)").unwrap();
}

#[derive(Serialize)]
struct VistEvent {
    ip: String,
    country: String,
    member: Membership,
}

#[derive(Debug)]
pub enum VisitorType {
    Referrer,
    Badge,
    ICON,
}

pub struct Context {
    pub badge: BoringFace,
    pub favicon: BoringFace,
    pub icon: BoringFace,

    pub db_pool: DbPool,
    pub unique_visitor: RwLock<HashMap<i64, i64>>,
    pub referrer: RwLock<HashMap<i64, i64>>,
    pub rank_svg: RwLock<i64>,

    pub domain2id: HashMap<String, i64>,
    pub id2member: HashMap<i64, Membership>,

    pub visitor_tx: Sender<String>,
    pub visitor_rx: Receiver<String>,

    pub rank: RwLock<Vec<Statistics>>,

    pub cache: r_cache::cache::Cache<String, ()>,
}

impl Context {
    pub async fn get_tend_from_uv_and_rv(&self, uv: i64, rv: i64) -> i64 {
        let tend = (uv + rv) / self.rank_svg.read().await.to_owned();
        if tend > 10 {
            return 10;
        } else if tend < 1 {
            return 1;
        }
        tend
    }

    pub async fn boring_visitor(
        &self,
        v_type: VisitorType,
        domain: &str,
        headers: &HeaderMap,
    ) -> Result<(&str, i64, i64, i64), anyhow::Error> {
        if let Some(id) = self.domain2id.get(domain) {
            let ip =
                String::from_utf8(headers.get("CF-Connecting-IP").unwrap().as_bytes().to_vec())
                    .unwrap();
            info!("ip {}", ip);

            let country =
                String::from_utf8(headers.get("CF-IPCountry").unwrap().as_bytes().to_vec())
                    .unwrap();
            info!("country {}", country);

            let visitor_key = format!("{}_{}_{:?}", ip, id, v_type);
            let visitor_cache = self.cache.get(&visitor_key).await;

            if visitor_cache.is_none() {
                self.cache
                    .set(visitor_key, (), Some(Duration::from_secs(60 * 60 * 4)))
                    .await;
            }

            let mut notification = false;

            let mut referrer = self.referrer.write().await;
            let mut dist_r = referrer.get(id).or(Some(&0)).unwrap().clone();
            if matches!(v_type, VisitorType::Referrer) && visitor_cache.is_none() {
                dist_r = dist_r + 1;
                referrer.insert(id.clone(), dist_r);
            }
            drop(referrer);

            let mut uv = self.unique_visitor.write().await;
            let mut dist_uv = uv.get(id).or(Some(&0)).unwrap().clone();
            if matches!(v_type, VisitorType::Badge) {
                if visitor_cache.is_none() {
                    dist_uv = dist_uv + 1;
                    uv.insert(id.clone(), dist_uv);
                }
                notification = true;
            }
            drop(uv);

            let tend = self.get_tend_from_uv_and_rv(dist_uv, dist_r).await;

            if notification {
                // 广播访客信息
                let mut member = self.id2member.get(id).unwrap().to_owned();
                member.description = "".to_string();
                member.icon = "".to_string();
                member.github_username = "".to_string();

                let _ = self.visitor_tx.send(
                    serde_json::json!(VistEvent {
                        ip: IPV6_MASK
                            .replace_all(
                                &IPV4_MASK.replace_all(&ip, "$1****$2").to_string(),
                                "$1****$2"
                            )
                            .to_string(),
                        country,
                        member,
                    })
                    .to_string(),
                );
            }

            return Ok((&self.id2member.get(id).unwrap().name, dist_uv, dist_r, tend));
        }
        return Err(anyhow!("not a member"));
    }

    pub async fn default(db_pool: DbPool) -> Context {
        let statistics = Statistics::today(db_pool.get().unwrap()).unwrap_or_default();

        let mut page_view: HashMap<i64, i64> = HashMap::new();
        let mut referrer: HashMap<i64, i64> = HashMap::new();

        statistics.iter().for_each(|s| {
            page_view.insert(s.membership_id, s.unique_visitor);
            referrer.insert(s.membership_id, s.referrer);
        });

        let mut membership: HashMap<i64, Membership> =
            serde_json::from_str(&fs::read_to_string("./resources/membership.json").unwrap())
                .unwrap();
        let mut domain2id: HashMap<String, i64> = HashMap::new();
        membership.iter_mut().for_each(|(k, v)| {
            v.id = k.clone(); // 将 ID 补给 member
            domain2id.insert(v.domain.clone(), k.clone());
        });

        let rank = Statistics::rank_between(
            db_pool.get().unwrap(),
            NaiveDateTime::from_timestamp(0, 0),
            now_shanghai(),
        )
        .unwrap();

        let (visitor_tx, visitor_rx) = watch::channel::<String>("".to_string());

        let rank_svg = Statistics::prev_day_rank_avg(db_pool.get().unwrap());

        Context {
            badge: BoringFace::new("#d0273e".to_string(), "#f5acb9".to_string(), true),
            favicon: BoringFace::new("#f5acb9".to_string(), "#d0273e".to_string(), false),
            icon: BoringFace::new("#d0273e".to_string(), "#f5acb9".to_string(), false),
            db_pool,

            unique_visitor: RwLock::new(page_view),
            referrer: RwLock::new(referrer),
            rank_svg: RwLock::new(rank_svg),
            rank: RwLock::new(rank),

            domain2id: domain2id,
            id2member: membership,

            visitor_rx,
            visitor_tx,

            cache: r_cache::cache::Cache::new(Some(Duration::from_secs(60 * 10))),
        }
    }

    // 每五分钟存一次，发现隔天刷新
    pub async fn save_per_5_minutes(&self) {
        let mut uv_cache: HashMap<i64, i64> = HashMap::new();
        let mut referrer_cache: HashMap<i64, i64> = HashMap::new();
        let mut changed_list: Vec<i64> = Vec::new();
        let mut _today = NaiveDateTime::new(now_shanghai().date(), NaiveTime::from_hms(0, 0, 0));
        let id_list = Vec::from_iter(self.id2member.keys());
        loop {
            tokio::time::sleep(Duration::from_secs(60 * 5)).await;
            changed_list.clear();
            // 对比是否有数据更新
            let mut uv_write = self.unique_visitor.write().await;
            let mut referrer_write = self.referrer.write().await;
            id_list.iter().for_each(|id| {
                let uv = uv_cache.get(id).unwrap_or(&0).clone();
                let new_uv = uv_write.get(id).unwrap_or(&0).clone();
                if uv.ne(&new_uv) {
                    uv_cache.insert(id.clone().clone(), new_uv);
                    changed_list.push(id.clone().clone());
                }
                let referrer = referrer_cache.get(id).unwrap_or(&0).clone();
                let new_referrer = referrer_write.get(id).unwrap_or(&0).clone();
                if referrer.ne(&new_referrer) {
                    referrer_cache.insert(id.clone().clone(), new_referrer);
                    if !changed_list.contains(id) {
                        changed_list.push(id.clone().clone());
                    }
                }
            });
            // 更新到数据库
            changed_list.iter().for_each(|id| {
                Statistics::insert_or_update(
                    self.db_pool.get().unwrap(),
                    &Statistics {
                        created_at: _today,
                        updated_at: now_shanghai(),
                        membership_id: id.clone(),
                        unique_visitor: uv_cache.get(id).unwrap_or(&0).clone(),
                        referrer: referrer_cache.get(id).unwrap_or(&0).clone(),
                        id: 0,
                    },
                )
                .unwrap();
            });
            let new_day = NaiveDateTime::new(now_shanghai().date(), NaiveTime::from_hms(0, 0, 0));
            if new_day.ne(&_today) {
                _today = new_day;
                // 如果是跨天重置数据
                uv_write.clear();
                referrer_write.clear();
                uv_cache.clear();
                referrer_cache.clear();
                // 重置访问打点
                self.cache.clear().await;
                // 更新上日访问量均值
                let mut rank_svg = self.rank_svg.write().await;
                *rank_svg = Statistics::prev_day_rank_avg(self.db_pool.get().unwrap());
            }
            drop(uv_write);
            drop(referrer_write);

            let mut rank = self.rank.write().await;
            *rank = Statistics::rank_between(
                self.db_pool.get().unwrap(),
                NaiveDateTime::from_timestamp(0, 0),
                now_shanghai(),
            )
            .unwrap_or_default();
        }
    }
}
