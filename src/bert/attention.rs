// Copyright 2019-present, the HuggingFace Inc. team, The Google AI Language Team and Facebook, Inc.
// Copyright (c) 2018, NVIDIA CORPORATION.  All rights reserved.
// Copyright 2019 Guillaume Becquin
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//     http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::dropout::Dropout;
use tch::{nn, Tensor};
use tch::kind::Kind::Float;
use crate::bert::bert::{Activation, BertConfig};
use crate::common::activations::{_gelu, _relu, _mish};

#[derive(Debug)]
pub struct BertSelfAttention {
    num_attention_heads: i64,
    attention_head_size: i64,
    dropout: Dropout,
    output_attentions: bool,
    query: nn::Linear,
    key: nn::Linear,
    value: nn::Linear,
}

impl BertSelfAttention {
    pub fn new(p: nn::Path, config: &BertConfig) -> BertSelfAttention {
        assert_eq!(config.hidden_size % config.num_attention_heads, 0, "Hidden size not a multiple of the number of attention heads");

        let query = nn::linear(&p / "query", config.hidden_size, config.hidden_size, Default::default());
        let key = nn::linear(&p / "key", config.hidden_size, config.hidden_size, Default::default());
        let value = nn::linear(&p / "value", config.hidden_size, config.hidden_size, Default::default());

        let dropout = Dropout::new(config.attention_probs_dropout_prob);
        let attention_head_size = config.hidden_size / config.num_attention_heads;
        let output_attentions = match config.output_attentions {
            Some(value) => value,
            None => false
        };

        BertSelfAttention {
            num_attention_heads: config.num_attention_heads,
            attention_head_size,
            dropout,
            output_attentions,
            query,
            key,
            value,
        }
    }

    fn split_heads(&self, x: Tensor, bs: i64, dim_per_head: i64) -> Tensor {
        x.view((bs, -1, self.num_attention_heads, dim_per_head)).transpose(1, 2)
    }

    fn flatten(&self, x: Tensor, bs: i64, dim_per_head: i64) -> Tensor {
        x.transpose(1, 2).contiguous().view((bs, -1, &self.num_attention_heads * dim_per_head))
    }

    pub fn forward_t(&self,
                     hidden_states: &Tensor,
                     mask: &Option<Tensor>,
                     encoder_hidden_states: &Option<Tensor>,
                     encoder_mask: &Option<Tensor>,
                     train: bool) -> (Tensor, Option<Tensor>) {
        let (key_layer, value_layer, mask) = match encoder_hidden_states {
            Some(encoder_hidden_state_values) => {
                (encoder_hidden_state_values.apply(&self.key),
                 encoder_hidden_state_values.apply(&self.value),
                 encoder_mask)
            }
            None => {
                (hidden_states.apply(&self.key),
                 hidden_states.apply(&self.value),
                 mask)
            }
        };

        let bs = hidden_states.size()[0];

        let query_layer = self.split_heads(hidden_states.apply(&self.query), bs, self.attention_head_size);
        let key_layer = self.split_heads(key_layer, bs, self.attention_head_size);
        let value_layer = self.split_heads(value_layer, bs, self.attention_head_size);
        let query_layer: Tensor = query_layer / (self.attention_head_size as f64).sqrt();

        let scores = if let Some(mask) = mask {
            query_layer.matmul(&key_layer.transpose(-1, -2)) + mask
        } else {
            query_layer.matmul(&key_layer.transpose(-1, -2))
        };

        let weights = scores.softmax(-1, Float).apply_t(&self.dropout, train);
        let context = self.flatten(weights.matmul(&value_layer), bs, self.attention_head_size);

        if !self.output_attentions {
            (context, None)
        } else {
            (context, Some(weights))
        }
    }
}

#[derive(Debug)]
pub struct BertSelfOutput {
    linear: nn::Linear,
    layer_norm: nn::LayerNorm,
    dropout: Dropout,
}

impl BertSelfOutput {
    pub fn new(p: &nn::Path, config: &BertConfig) -> BertSelfOutput {
        let linear = nn::linear(p / "dense", config.hidden_size, config.hidden_size, Default::default());
        let layer_norm_config = nn::LayerNormConfig { eps: 1e-12, ..Default::default() };
        let layer_norm = nn::layer_norm(p / "LayerNorm", vec![config.hidden_size], layer_norm_config);
        let dropout = Dropout::new(config.hidden_dropout_prob);

        BertSelfOutput { linear, layer_norm, dropout }
    }

    pub fn forward_t(&self, hidden_states: &Tensor, input_tensor: &Tensor, train: bool) -> Tensor {
        let hidden_states: Tensor = input_tensor + hidden_states.apply(&self.linear).apply_t(&self.dropout, train);
        hidden_states.apply(&self.layer_norm)
    }
}

#[derive(Debug)]
pub struct BertAttention {
    _self: BertSelfAttention,
    output: BertSelfOutput,
}

impl BertAttention {
    pub fn new(p: &nn::Path, config: &BertConfig) -> BertAttention {
        let _self = BertSelfAttention::new(p / "self", config);
        let output = BertSelfOutput::new(&(p / "output"), config);
        BertAttention { _self, output }
    }

    pub fn forward_t(&self,
                     hidden_states: &Tensor,
                     mask: &Option<Tensor>,
                     encoder_hidden_states: &Option<Tensor>,
                     encoder_mask: &Option<Tensor>,
                     train: bool) -> (Tensor, Option<Tensor>) {
        let (self_output, attention_weights) = self._self.
            forward_t(hidden_states, mask, encoder_hidden_states, encoder_mask, train);

        let self_output = self.output.forward_t(&self_output, hidden_states, train);
        (self_output, attention_weights)
    }
}

pub struct BertIntermediate {
    lin: nn::Linear,
    activation: Box<dyn Fn(&Tensor) -> Tensor>,
}

impl BertIntermediate {
    pub fn new(p: &nn::Path, config: &BertConfig) -> BertIntermediate {
        let lin = nn::linear(p / "dense", config.hidden_size, config.intermediate_size, Default::default());
        let activation = Box::new(match &config.hidden_act {
            Activation::gelu => _gelu,
            Activation::relu => _relu,
            Activation::mish => _mish
        });
        BertIntermediate { lin, activation }
    }

    pub fn forward(&self, hidden_states: &Tensor) -> Tensor {
        (self.activation)(&hidden_states.apply(&self.lin))
    }
}

pub struct BertOutput {
    lin: nn::Linear,
    layer_norm: nn::LayerNorm,
    dropout: Dropout,
}

impl BertOutput {
    pub fn new(p: &nn::Path, config: &BertConfig) -> BertOutput {
        let lin = nn::linear(p / "dense", config.intermediate_size, config.hidden_size, Default::default());
        let layer_norm_config = nn::LayerNormConfig { eps: 1e-12, ..Default::default() };
        let layer_norm = nn::layer_norm(p / "LayerNorm", vec![config.hidden_size], layer_norm_config);
        let dropout = Dropout::new(config.hidden_dropout_prob);

        BertOutput { lin, layer_norm, dropout }
    }

    pub fn forward_t(&self, hidden_states: &Tensor, input_tensor: &Tensor, train: bool) -> Tensor {
        let hidden_states: Tensor = input_tensor + hidden_states.apply(&self.lin).apply_t(&self.dropout, train);
        hidden_states.apply(&self.layer_norm)
    }
}

