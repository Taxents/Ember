//! Radio Garden parsing, from responses saved (and trimmed) in `fixtures/radio_*.json`.

use music_core::radio::{self, Section};
use serde_json::Value;
use std::path::PathBuf;

fn fixture(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(&p).expect("fixture")).expect("json")
}

#[test]
fn places_list() {
    let places = radio::parse_places(&fixture("radio_places.json"));
    assert!(places.len() > 30);
    let london = places.iter().find(|p| p.title == "London").unwrap();
    assert_eq!(london.id, "0eZoYyEW");
    assert_eq!(london.country, "United Kingdom");
    assert!(london.size > 100);
    assert!((london.lat - 51.5).abs() < 0.1 && (london.lon + 0.1).abs() < 0.1);

    let countries = radio::countries(&places);
    let uk = countries
        .iter()
        .find(|c| c.name == "United Kingdom")
        .unwrap();
    assert!(uk.places > 1 && uk.stations >= london.size);
    assert!(countries.windows(2).all(|w| w[0].stations >= w[1].stations));

    let in_uk = radio::places_in(&places, "United Kingdom");
    assert_eq!(in_uk[0].title, "London", "biggest first");
    assert!(in_uk.iter().all(|p| p.country == "United Kingdom"));

    // From central London, London is "near you"; from the middle of nowhere, the nearest.
    assert_eq!(radio::nearest(&places, 51.5, -0.12).unwrap().id, "0eZoYyEW");
}

#[test]
fn place_page() {
    let p = radio::parse_page("0eZoYyEW", &fixture("radio_page_place.json")).unwrap();
    assert_eq!(
        (p.title.as_str(), p.subtitle.as_str()),
        ("London", "United Kingdom")
    );
    assert!(p.place);
    assert!(p.count > 100);
    let titles: Vec<&str> = p
        .sections
        .iter()
        .map(|s| match s {
            Section::Stations { title, .. } | Section::Places { title, .. } => title.as_str(),
        })
        .collect();
    assert_eq!(titles[0], "Stations in London");
    let Section::Stations { stations, .. } = &p.sections[0] else {
        panic!("stations first");
    };
    let st = &stations[0];
    assert_eq!(st.id, "QenbpKkf");
    assert_eq!(st.title, "Crackers Radio");
    assert_eq!(st.subtitle(), "London, United Kingdom");
    assert_eq!(st.place_id.as_deref(), Some("0eZoYyEW"));
    let nearby = p
        .sections
        .iter()
        .find_map(|s| match s {
            Section::Places { title, places } if title.starts_with("Nearby") => Some(places),
            _ => None,
        })
        .unwrap();
    assert!(nearby.iter().all(|p| !p.country && !p.id.is_empty()));
}

#[test]
fn country_page() {
    let p = radio::parse_page("jm6Gyxm7", &fixture("radio_page_country.json")).unwrap();
    assert_eq!(p.title, "United Kingdom");
    assert!(
        !p.place,
        "a country page points at its capital's map, not itself"
    );
    assert!(matches!(&p.sections[0], Section::Stations { stations, .. } if !stations.is_empty()));
    assert!(p.sections.iter().any(
        |s| matches!(s, Section::Places { places, .. } if places.iter().any(|p| p.title == "London" && p.count > 100))
    ));
}

#[test]
fn search_results() {
    let (stations, places) = radio::parse_search(&fixture("radio_search.json"));
    assert!(!stations.is_empty());
    assert!(
        stations
            .iter()
            .all(|s| radio::valid_id(&s.id) && !s.title.is_empty())
    );
    let germany = places.iter().find(|p| p.title == "Germany").unwrap();
    assert!(germany.country);
    assert_eq!(germany.id, "gz1nOd3X");
    let berlin = places.iter().find(|p| p.title == "Berlin").unwrap();
    assert!(!berlin.country);
    assert_eq!(berlin.subtitle, "Germany");
    assert!(berlin.count > 100);
}
