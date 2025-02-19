/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use tokio::sync::watch;

use crate::DirectorySchedule;

impl DirectorySchedule {
    pub fn spawn(self, mut shutdown_rx: watch::Receiver<bool>) {
        tracing::debug!("Directory query scheduler task starting.");
        tokio::spawn(async move {
            loop {
                if tokio::time::timeout(self.cron.time_to_next(), shutdown_rx.changed())
                    .await
                    .is_ok()
                {
                    tracing::debug!("Directory query scheduler task exiting.");
                    return;
                }

                for query in &self.query {
                    if let Err(err) = self.directory.query(query, &[]).await {
                        tracing::warn!(
                            context = "directory-scheduler",
                            event = "error",
                            query = query,
                            reason = ?err,
                        );
                    } else {
                        tracing::debug!(
                            context = "directory-scheduler",
                            event = "success",
                            query = query,
                        );
                    }
                }
            }
        });
    }
}
