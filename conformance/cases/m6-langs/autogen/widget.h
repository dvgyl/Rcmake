#pragma once
#include <QWidget>

class Widget final : public QWidget {
    Q_OBJECT
public:
    explicit Widget(QWidget *parent = nullptr);
    int labelCount() const;
};
