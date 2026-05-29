/*!
DashScope ASR provider。

这一版实现先不重写旧链路的细节，而是复用已经存在的 `PipelineIo`：

1. 上传音频到百炼临时 OSS；
2. 提交百炼转写任务；
3. 轮询转写结果；
4. 返回 `NormalizedTranscript`。

这样能在最小改动下先完成 provider 抽象，保证旧功能不回退。
*/

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    error::AppResult,
    models::NormalizedTranscript,
    pipeline::PipelineIo,
};

use super::{
    provider::AsrProvider,
    types::AsrInput,
};

pub struct DashScopeAsrProvider {
    pipeline: Arc<dyn PipelineIo>,
}

impl DashScopeAsrProvider {
    pub fn new(pipeline: Arc<dyn PipelineIo>) -> Self {
        Self { pipeline }
    }

    /// 当前阶段仍使用旧的 pipeline 云端链路，后续再按需要继续细拆。
    async fn transcribe_with_pipeline(
        &self,
        input: &AsrInput,
    ) -> AppResult<NormalizedTranscript> {
        let uploaded_source_url = self
            .pipeline
            .upload_audio_for_transcription(&input.audio_path, None)
            .await?;
        let task_id = self
            .pipeline
            .submit_transcription_task(&uploaded_source_url)
            .await?;
        self.pipeline.poll_transcription_task(&task_id).await
    }
}

#[async_trait]
impl AsrProvider for DashScopeAsrProvider {
    async fn transcribe_audio(&self, input: AsrInput) -> AppResult<NormalizedTranscript> {
        self.transcribe_with_pipeline(&input).await
    }
}
