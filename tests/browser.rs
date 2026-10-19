use claud2api::{
    browser::{BrowserProfile, TransportSettings},
    config::Settings,
};

#[test]
fn catalog_defaults_and_proxy_validation_are_explicit() {
    assert!(Settings::default().simulated_cache);
    assert_eq!(BrowserProfile::ALL.len(), 15);
    for profile in BrowserProfile::ALL {
        assert_eq!(BrowserProfile::parse(profile.id()).unwrap(), *profile);
        assert_eq!(
            serde_json::from_str::<BrowserProfile>(&serde_json::to_string(profile).unwrap())
                .unwrap(),
            *profile
        );
    }
    assert!(BrowserProfile::parse("firefox_152_windows").is_err());
    for url in [
        "http://user:p%40ss@127.0.0.1:8080",
        "https://proxy.example",
        "socks5://127.0.0.1:1080",
        "socks5h://user:pass@proxy.example:1080",
    ] {
        let settings = TransportSettings {
            browser_profile: None,
            proxy_url: Some(url.into()),
        };
        settings.validate().unwrap();
        let public = settings.public_proxy().to_string();
        assert!(!public.contains("p%40ss") && !public.contains("pass") && !public.contains("user"));
    }
    for url in [
        "ftp://proxy.example:21",
        "socks5://proxy.example",
        "http://proxy.example/path",
        "http://proxy.example/?token=secret",
        "http://proxy.example/#secret",
        "http://\r\nsecret",
        "secret",
    ] {
        let settings = TransportSettings {
            browser_profile: None,
            proxy_url: Some(url.into()),
        };
        let error = settings.validate().unwrap_err();
        assert!(!error.message.contains("secret"));
    }
}
