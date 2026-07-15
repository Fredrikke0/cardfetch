use super::{Store, StoreResult};
use anyhow::Context;
use serde::Deserialize;
use std::time::Duration;

const GRAPHQL_URL: &str = "https://www.outland.no/api/graphql";
const STORE_NAME: &str = "outland.no";
const STORE_BASE_URL: &str = "https://www.outland.no";
const TIMEOUT_SECS: u64 = 30;
const CATEGORY_UID: &str = "MTI1MA==";

pub struct Outland;

impl Outland {
    pub fn new() -> Self {
        Outland
    }
}

impl Store for Outland {
    fn name(&self) -> &str {
        STORE_NAME
    }

    fn timeout_secs(&self) -> u64 {
        TIMEOUT_SECS
    }

    fn search(
        &self,
        client: &reqwest::blocking::Client,
        card_name: &str,
    ) -> anyhow::Result<Vec<StoreResult>> {
        let all_items = fetch_all_pages(client, card_name)?;

        let item = match all_items
            .iter()
            .find(|item| names_match(card_name, &item.name))
        {
            Some(item) => item,
            None => return Ok(vec![]),
        };

        let price = &item.price_range.minimum_price.final_price;
        let price_oere = (price.value * 100.0).round() as u32;
        Ok(vec![StoreResult {
            store_name: STORE_NAME.to_string(),
            card_name: card_name.to_string(),
            price: price_oere,
            url: format!("{}/{}", STORE_BASE_URL, item.url_key),
        }])
    }
}

// ── GraphQL types (outland-specific, private) ────────────────────────────

const PRODUCT_LIST_QUERY: &str = r#"query ProductList($pageSize: Int = 24, $currentPage: Int = 1, $filters: ProductAttributeFilterInput = {}, $sort: ProductAttributeSortInput = {}, $search: String = "", $onlyItems: Boolean = false) {
  products(
    pageSize: $pageSize
    currentPage: $currentPage
    filter: $filters
    sort: $sort
    search: $search
  ) {
    items {
      __typename
      uid
      url_key
      sku
      name
      new_to_date
      new_from_date
      small_image {
        url
        label
        disabled
        sizes {
          width
          height
          __typename
        }
        __typename
      }
      price_range {
        minimum_price {
          regular_price {
            currency
            value
            __typename
          }
          discount {
            amount_off
            percent_off
            __typename
          }
          final_price {
            currency
            value
            __typename
          }
          __typename
        }
        __typename
      }
      categories {
        uid
        name
        url_path
        __typename
      }
      is_preorder
      stock_status
      only_x_left_in_stock
      attribute_set_id
      publisher: custom_attributeV2(attribute_code: "publisher") {
        ... on AttributeValue {
          value
          __typename
        }
        ... on AttributeSelectedOptions {
          selected_options {
            label
            __typename
          }
          __typename
        }
        __typename
      }
      bookAuthor: custom_attributeV2(attribute_code: "book_author") {
        ... on AttributeValue {
          value
          __typename
        }
        ... on AttributeSelectedOptions {
          selected_options {
            label
            __typename
          }
          __typename
        }
        __typename
      }
      volume: custom_attributeV2(attribute_code: "volume") {
        ... on AttributeValue {
          value
          __typename
        }
        __typename
      }
      bookSeries: custom_attributeV2(attribute_code: "book_series") {
        ... on AttributeValue {
          value
          __typename
        }
        ... on AttributeSelectedOptions {
          selected_options {
            label
            __typename
          }
          __typename
        }
        __typename
      }
      language: custom_attributeV2(attribute_code: "language") {
        ... on AttributeValue {
          value
          __typename
        }
        ... on AttributeSelectedOptions {
          selected_options {
            label
            __typename
          }
          __typename
        }
        __typename
      }
      bookCover: custom_attributeV2(attribute_code: "book_cover") {
        ... on AttributeValue {
          value
          __typename
        }
        ... on AttributeSelectedOptions {
          selected_options {
            label
            __typename
          }
          __typename
        }
        __typename
      }
      member_price {
        currency
        value
        __typename
      }
      id
      rating_summary
      ... on ConfigurableProduct {
        configurable_options {
          attribute_code
          uid
          label
          values {
            store_label
            uid
            swatch_data {
              __typename
              ... on TextSwatchData {
                value
                __typename
              }
              ... on ColorSwatchData {
                value
                __typename
              }
              ... on ImageSwatchData {
                value
                thumbnail
                __typename
              }
            }
            __typename
          }
          __typename
        }
        __typename
      }
    }
    suggestions @skip(if: $onlyItems) {
      search
      __typename
    }
    aggregations(filter: {category: {includeDirectChildrenOnly: true}}) @skip(if: $onlyItems) {
      __typename
      label
      attribute_code
      count
      options {
        __typename
        label
        value
        count
      }
      has_more
    }
    page_info @skip(if: $onlyItems) {
      current_page
      total_pages
      __typename
    }
    total_count @skip(if: $onlyItems)
    sort_fields @skip(if: $onlyItems) {
      default
      options {
        label
        value
        __typename
      }
      __typename
    }
    __typename
  }
}"#;

#[derive(serde::Serialize)]
struct GraphQLRequest {
    #[serde(rename = "operationName")]
    operation_name: &'static str,
    variables: ProductListVariables,
    extensions: Extensions,
    query: &'static str,
}

#[derive(serde::Serialize)]
struct ProductListVariables {
    #[serde(rename = "pageSize")]
    page_size: u32,
    #[serde(rename = "currentPage")]
    current_page: u32,
    filters: FilterInput,
    sort: SortInput,
    search: String,
    #[serde(rename = "onlyItems")]
    only_items: bool,
}

#[derive(serde::Serialize)]
struct FilterInput {
    category_uid: CategoryUidFilter,
    in_stock: InStockFilter,
}

#[derive(serde::Serialize)]
struct CategoryUidFilter {
    #[serde(rename = "in")]
    r#in: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct InStockFilter {
    #[serde(rename = "in")]
    r#in: Vec<&'static str>,
}

#[derive(serde::Serialize)]
struct SortInput {
    relevance: &'static str,
}

#[derive(serde::Serialize)]
struct Extensions {
    #[serde(rename = "clientLibrary")]
    client_library: ClientLibrary,
}

#[derive(serde::Serialize)]
struct ClientLibrary {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Deserialize)]
struct GraphQLResponse {
    data: Option<ProductListData>,
}

#[derive(Debug, Deserialize)]
struct ProductListData {
    products: Products,
}

#[derive(Debug, Deserialize)]
struct Products {
    items: Vec<ProductItem>,
    page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
struct PageInfo {
    total_pages: u32,
}

#[derive(Debug, Deserialize)]
struct ProductItem {
    url_key: String,
    name: String,
    price_range: PriceRange,
}

#[derive(Debug, Deserialize)]
struct PriceRange {
    minimum_price: MinimumPrice,
}

#[derive(Debug, Deserialize)]
struct MinimumPrice {
    final_price: Money,
}

#[derive(Debug, Deserialize)]
struct Money {
    value: f64,
}

// ── Request builder ──────────────────────────────────────────────────────

fn build_request(search_term: &str, page: u32) -> GraphQLRequest {
    GraphQLRequest {
        operation_name: "ProductList",
        variables: ProductListVariables {
            page_size: 48,
            current_page: page,
            filters: FilterInput {
                category_uid: CategoryUidFilter {
                    r#in: vec![CATEGORY_UID],
                },
                in_stock: InStockFilter { r#in: vec!["1"] },
            },
            sort: SortInput { relevance: "DESC" },
            search: search_term.to_string(),
            only_items: false,
        },
        extensions: Extensions {
            client_library: ClientLibrary {
                name: "@apollo/client",
                version: "4.0.11",
            },
        },
        query: PRODUCT_LIST_QUERY,
    }
}

// ── Pagination ───────────────────────────────────────────────────────────

fn fetch_all_pages(
    client: &reqwest::blocking::Client,
    search_term: &str,
) -> anyhow::Result<Vec<ProductItem>> {
    let mut all_items: Vec<ProductItem> = Vec::new();
    let mut current_page = 1u32;

    loop {
        let request = build_request(search_term, current_page);

        let response = client
            .post(GRAPHQL_URL)
            .json(&request)
            .send()
            .context("Failed to send GraphQL request")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "GraphQL request returned HTTP {}",
                response.status().as_u16()
            );
        }

        let body: GraphQLResponse = response
            .json()
            .context("Failed to parse GraphQL response JSON")?;

        let data = body
            .data
            .context("GraphQL response contained no data field")?;

        let products = data.products;
        let total_pages = products.page_info.total_pages;

        all_items.extend(products.items);

        if current_page >= total_pages {
            break;
        }

        current_page += 1;
        std::thread::sleep(Duration::from_millis(super::DELAY_MS));
    }

    Ok(all_items)
}

// ── Name matching (outland-specific) ─────────────────────────────────────

fn strip_enkeltkort(name: &str) -> &str {
    let suffixes = [" (Enkeltkort)", " (enkeltkort)"];
    for suffix in &suffixes {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped;
        }
    }
    name
}

fn names_match(searched: &str, product_name: &str) -> bool {
    let stripped = strip_enkeltkort(product_name);
    searched.eq_ignore_ascii_case(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_enkeltkort() {
        assert_eq!(
            strip_enkeltkort("Snakeskin Veil (Enkeltkort)"),
            "Snakeskin Veil"
        );
        assert_eq!(
            strip_enkeltkort("Snakeskin Veil (enkeltkort)"),
            "Snakeskin Veil"
        );
        assert_eq!(strip_enkeltkort("Snakeskin Veil"), "Snakeskin Veil");
    }

    #[test]
    fn test_names_match() {
        assert!(names_match("Snakeskin Veil", "Snakeskin Veil (Enkeltkort)"));
        assert!(names_match("snakeskin veil", "Snakeskin Veil (Enkeltkort)"));
        assert!(names_match("Snakeskin Veil", "Snakeskin Veil"));
        assert!(!names_match("Snakeskin", "Snakeskin Veil (Enkeltkort)"));
        assert!(!names_match("Other Card", "Snakeskin Veil (Enkeltkort)"));
    }

    #[test]
    fn test_store_name() {
        let store = Outland::new();
        assert_eq!(store.name(), "outland.no");
    }
}
